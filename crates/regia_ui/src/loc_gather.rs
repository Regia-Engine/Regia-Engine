// SPDX-License-Identifier: MPL-2.0
//! The gather tool (`docs/specs/content-data.md` § 1.7, ADR-1466): finds the localization keys that
//! content and code use and writes the missing ones into each locale's `gathered.ftl`.
//!
//! Keys are found by text scan, not by loading assets: `Loc("key")` and `"loc:key"` in `.ron`
//! files (UI layouts, scenes, any asset), and `loc!("key"` in `.rs` files. A key that is not a
//! Fluent identifier is reported, not gathered. Existing messages are never rewritten; new ones
//! are appended (the source locale gets the key as placeholder text, other locales a copy of the
//! source message under a `# TODO translate` comment, so every file stays valid Fluent).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::loc_check::ParsedLocale;
use crate::loc_config::GatherConfig;

/// The file each locale's gathered keys go to.
pub const GATHERED_FILE: &str = "gathered.ftl";

/// Where a key was found.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct KeyUse {
    /// File path relative to the project root, `/` separators.
    pub file: String,
    /// 1-based line.
    pub line: u32,
}

/// What a gather found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Gathered {
    /// Every key with the places that use it (sorted).
    pub keys: BTreeMap<String, Vec<KeyUse>>,
    /// Text in a key position that is not a Fluent identifier.
    pub bad_keys: Vec<(KeyUse, String)>,
    /// Extra comment lines (without `# `) written above a key's message by [`plan`], for keys a
    /// tool supplied through [`Gathered::add_key`] (conversations: cast note, speaker, budget).
    pub notes: BTreeMap<String, Vec<String>>,
    /// Files read.
    pub files_scanned: usize,
}

impl Gathered {
    /// The key names.
    #[must_use]
    pub fn key_set(&self) -> BTreeSet<String> {
        self.keys.keys().cloned().collect()
    }

    /// Records a key found by another tool: where it is used and the comment lines to write
    /// above its message in `gathered.ftl`.
    pub fn add_key(&mut self, key: &str, file: &str, line: u32, notes: Vec<String>) {
        if is_message_id(key) {
            self.keys.entry(key.to_owned()).or_default().push(KeyUse {
                file: file.to_owned(),
                line,
            });
            if !notes.is_empty() {
                self.notes.entry(key.to_owned()).or_insert(notes);
            }
        }
    }

    fn add(&mut self, file: &str, found: Vec<(String, u32)>) {
        for (key, line) in found {
            let at = KeyUse {
                file: file.to_owned(),
                line,
            };
            if is_message_id(&key) {
                self.keys.entry(key).or_default().push(at);
            } else {
                self.bad_keys.push((at, key));
            }
        }
    }
}

/// Whether `key` is a Fluent message identifier: `[a-zA-Z][a-zA-Z0-9_-]*`.
#[must_use]
pub fn is_message_id(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The text between `"` at `start` (exclusive) and the next `"`; `None` when the line ends first.
fn quoted(text: &str, start: usize) -> Option<&str> {
    let rest = &text[start..];
    let end = rest.find(['"', '\n'])?;
    (rest.as_bytes()[end] == b'"').then(|| &rest[..end])
}

/// Whether the `r` at `i` begins a token: nothing identifier-like before it, or a `b`/`c` string
/// prefix that itself begins a token.
fn raw_prefix_starts_token(b: &[u8], i: usize) -> bool {
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    match i.checked_sub(1).map(|p| b[p]) {
        None => true,
        Some(b'b' | b'c') => i.checked_sub(2).is_none_or(|p| !ident(b[p])),
        Some(prev) => !ident(prev),
    }
}

/// `text` with every `/* ... */` block comment (nested, as Rust and RON nest them) blanked to
/// spaces. Newlines and byte offsets are kept, so lines still count; string and character
/// literals and `//` comments are stepped over, so a `/*` inside them opens nothing. Raw strings
/// (`r"..."`, `r#"..."#`) are blanked too, so a `loc!` lookalike inside one is not gathered.
fn blank_block_comments(text: &str) -> std::borrow::Cow<'_, str> {
    // Nothing to blank: no block comment and no raw string (a file with only raw strings still
    // needs the walk, or their `loc!` lookalikes are gathered as phantom keys).
    if !text.contains("/*") && !text.contains("r\"") && !text.contains("r#") {
        return std::borrow::Cow::Borrowed(text);
    }
    let b = text.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0usize;
    while i < b.len() {
        // A raw string (`r"..."`, `r#"..."#`, any hash count) is checked before the normal
        // string rule: its content may hold quotes, `/*` and `loc!` lookalikes that must
        // neither end the skip early nor open a block comment, and a `loc!` inside one is
        // not a call. The whole literal is blanked (newlines kept, so lines still count).
        // `r#ident` (a raw identifier) is not a string: only a quote after the hashes
        // starts one. The `r` must start a token (or follow the `b`/`c` of `br"`/`cr"`): the
        // `r` that ends `"four"` or `bar"` is not a raw-string prefix.
        if b[i] == b'r' && raw_prefix_starts_token(b, i) {
            let mut j = i + 1;
            while b.get(j) == Some(&b'#') {
                j += 1;
            }
            if b.get(j) == Some(&b'"') {
                let hashes = j - (i + 1);
                for out_byte in out.iter_mut().take(j + 1).skip(i) {
                    *out_byte = b' ';
                }
                let mut p = j + 1;
                while p < b.len() {
                    if b[p] == b'"' {
                        let mut k = 0usize;
                        while k < hashes && b.get(p + 1 + k) == Some(&b'#') {
                            k += 1;
                        }
                        let close_len = if k == hashes { 1 + hashes } else { 1 };
                        for q in p..p + close_len {
                            if b[q] != b'\n' {
                                out[q] = b' ';
                            }
                        }
                        p += close_len;
                        if k == hashes {
                            break;
                        }
                    } else {
                        if b[p] != b'\n' {
                            out[p] = b' ';
                        }
                        p += 1;
                    }
                }
                i = p;
                continue;
            }
        }
        match (b[i], b.get(i + 1).copied()) {
            (b'/', Some(b'/')) => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            (b'/', Some(b'*')) => {
                let mut depth = 0usize;
                while i < b.len() {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        if b[i] != b'\n' {
                            out[i] = b' ';
                        }
                        i += 1;
                    }
                }
            }
            (b'"', _) => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            // A character literal (`'"'`, `'\''`); a lone `'` is a lifetime.
            (b'\'', Some(b'\\')) => {
                i += 2;
                while i < b.len() && b[i] != b'\'' && b[i] != b'\n' {
                    i += 1;
                }
                i += 1;
            }
            (b'\'', Some(_)) if b.get(i + 2) == Some(&b'\'') => i += 3,
            _ => i += 1,
        }
    }
    // Every byte of a comment became a space (newlines kept), so the text is still UTF-8.
    std::borrow::Cow::Owned(String::from_utf8(out).unwrap_or_else(|_| text.to_owned()))
}

/// Every `(key, line)` after `marker` + optional spaces + `"` in `text`, skipping lines whose
/// first non-space characters are `//` and `/* ... */` block comments.
fn scan(text: &str, marker: &str) -> Vec<(String, u32)> {
    let text = &*blank_block_comments(text);
    let mut out = Vec::new();
    let mut line_start = 0usize;
    for (n, line) in text.split_inclusive('\n').enumerate() {
        let line_no = u32::try_from(n + 1).unwrap_or(u32::MAX);
        if !line.trim_start().starts_with("//") {
            let mut from = 0usize;
            while let Some(at) = line[from..].find(marker) {
                let found = from + at;
                let after = found + marker.len();
                // `alloc!(` and `MyLoc(` are other names, not the marker.
                let inside_name = line[..found]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
                if inside_name {
                    from = after;
                    continue;
                }
                let spaces = line[after..].len() - line[after..].trim_start_matches([' ', '\t']).len();
                let quote = after + spaces;
                if line[quote..].starts_with('"')
                    && let Some(key) = quoted(text, line_start + quote + 1)
                {
                    out.push((key.to_owned(), line_no));
                }
                from = after;
            }
        }
        line_start += line.len();
    }
    out
}

/// Keys in a `.ron` file: `Loc("key")` and `"loc:key"`, outside `/* */` comments.
#[must_use]
pub fn scan_ron(text: &str) -> Vec<(String, u32)> {
    let text = &*blank_block_comments(text);
    let mut out = scan(text, "Loc(");
    let mut line_no = 1u32;
    let mut from = 0usize;
    while let Some(at) = text[from..].find("\"loc:") {
        let start = from + at;
        line_no += u32::try_from(text[from..start].matches('\n').count()).unwrap_or(0);
        let line_head = &text[text[..start].rfind('\n').map_or(0, |p| p + 1)..start];
        // A commented-out line is skipped here as it is for `Loc(`.
        if !line_head.trim_start().starts_with("//")
            && let Some(key) = quoted(text, start + "\"loc:".len())
        {
            out.push((key.to_owned(), line_no));
        }
        from = start + 1;
    }
    out.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    out
}

/// Keys in a `.rs` file: `loc!("key"` (also `regia_ui::loc!(`, spaces or tabs before the `(`, and
/// any whitespace, newlines too, between the `(` and the key), outside comments. The line is the
/// line of `loc!`.
#[must_use]
pub fn scan_rust(text: &str) -> Vec<(String, u32)> {
    let text = &*blank_block_comments(text);
    let mut out = Vec::new();
    let (mut from, mut line_no, mut counted) = (0usize, 1u32, 0usize);
    while let Some(at) = text[from..].find("loc!") {
        let start = from + at;
        from = start + "loc!".len();
        let line_start = text[..start].rfind('\n').map_or(0, |n| n + 1);
        if text[line_start..start].trim_start().starts_with("//") {
            continue;
        }
        // `alloc!(` and `my_loc!(` are other macros, not `loc!` (`regia_ui::loc!(` still is).
        if text[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        let Some(open) = text[from..].trim_start_matches([' ', '\t']).strip_prefix('(') else {
            continue;
        };
        let key_start = open.trim_start();
        if !key_start.starts_with('"') {
            continue;
        }
        if let Some(key) = quoted(text, text.len() - key_start.len() + 1) {
            line_no += u32::try_from(text[counted..start].matches('\n').count()).unwrap_or(0);
            counted = start;
            out.push((key.to_owned(), line_no));
        }
    }
    out.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    out
}

/// Walks `config.roots` under `root` and gathers keys from `.ron` and `.rs` files. Hidden entries,
/// `target/` and the `exclude` prefixes are skipped; unreadable files are skipped with a warning.
///
/// # Errors
///
/// A root that exists but cannot be listed.
pub fn gather_project(root: &Path, config: &GatherConfig) -> std::io::Result<Gathered> {
    let _span = tracing::info_span!(target: "regia_ui", "loc_gather").entered();
    let mut files = Vec::new();
    for dir in &config.roots {
        let start = root.join(dir);
        if start.is_dir() {
            walk(&start, &mut files)?;
        }
    }
    files.sort();
    // Overlapping roots (`assets` and `assets/ui`) reach a file twice; read it once.
    files.dedup();
    let mut out = Gathered::default();
    // `exclude` entries are project-relative prefixes; `\` separators and a trailing `/` (as
    // a user writes a folder) mean the same folder.
    let exclude: Vec<String> = config
        .exclude
        .iter()
        .map(|e| e.replace('\\', "/").trim_end_matches('/').to_owned())
        .filter(|e| !e.is_empty())
        .collect();
    for path in files {
        let rel = relative(root, &path);
        if exclude
            .iter()
            .any(|e| rel == *e || rel.starts_with(&format!("{e}/")))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            tracing::warn!(target: "regia_ui", "gather: cannot read {}", path.display());
            continue;
        };
        out.files_scanned += 1;
        let found = if rel.ends_with(".rs") {
            scan_rust(&text)
        } else {
            scan_ron(&text)
        };
        out.add(&rel, found);
    }
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        let path = entry.path();
        let mut kind = entry.file_type()?;
        let wanted = name.ends_with(".ron") || name.ends_with(".rs");
        if kind.is_symlink() {
            // A linked file is read through the link (a shared prefab); a linked directory is
            // not descended (it could loop) and says so. Both used to be skipped without a word
            // (audit-ext-b Low).
            match std::fs::metadata(&path) {
                Ok(target) if target.is_file() => kind = target.file_type(),
                Ok(target) if target.is_dir() => {
                    tracing::warn!(target: "regia_ui", "loc gather: not following the directory link {}", path.display());
                    continue;
                }
                _ => {
                    if wanted {
                        tracing::warn!(target: "regia_ui", "loc gather: skipping the broken link {}", path.display());
                    }
                    continue;
                }
            }
        }
        if kind.is_dir() {
            walk(&path, out)?;
        } else if kind.is_file() && wanted {
            out.push(path);
        }
    }
    Ok(())
}

fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// One file the gather tool will write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    /// The locale.
    pub locale: String,
    /// The file, relative to the project root.
    pub path: String,
    /// Keys added, sorted.
    pub added: Vec<String>,
    /// The whole new file text (existing text plus the appended messages).
    pub text: String,
}

/// The source text of message `key` in `files`, as Fluent writes it (`key = ...\n`).
fn message_source(files: &[(String, String)], key: &str) -> Option<String> {
    let mut found = None;
    for (_, text) in files {
        let resource = match fluent_syntax::parser::parse(text.as_str()) {
            Ok(resource) | Err((resource, _)) => resource,
        };
        for entry in resource.body {
            if let fluent_syntax::ast::Entry::Message(message) = &entry
                && message.id.name == key
            {
                let mut single = message.clone();
                single.comment = None;
                found = Some(fluent_syntax::serializer::serialize(
                    &fluent_syntax::ast::Resource {
                        body: vec![fluent_syntax::ast::Entry::Message(single)],
                    },
                ));
            }
        }
    }
    found
}

/// Plans the gathered writes: for each of `locales` (`(parsed, files)` with the files' current
/// text), the gathered keys it does not define itself are appended to its `gathered.ftl`.
/// `existing` maps a locale to the current text of its `gathered.ftl` (empty when absent).
#[must_use]
pub fn plan(
    gathered: &Gathered,
    locales: &[(ParsedLocale, Vec<(String, String)>)],
    source: &str,
    localization_dir: &str,
    existing: &BTreeMap<String, String>,
) -> Vec<PlannedFile> {
    let source_files = locales
        .iter()
        .find(|(p, _)| p.locale == source)
        .map(|(_, files)| files.as_slice())
        .unwrap_or(&[]);
    let mut out = Vec::new();
    for (parsed, _) in locales {
        let missing: Vec<&String> = gathered
            .keys
            .keys()
            .filter(|k| !parsed.messages.contains_key(*k))
            .collect();
        if missing.is_empty() {
            continue;
        }
        let mut text = existing.get(&parsed.locale).cloned().unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        for key in &missing {
            let uses = &gathered.keys[*key];
            if let Some(first) = uses.first() {
                text.push_str(&format!("\n# source: {}:{}\n", first.file, first.line));
            }
            for note in gathered.notes.get(*key).into_iter().flatten() {
                text.push_str(&format!("# {note}\n"));
            }
            let copied = (parsed.locale != source)
                .then(|| message_source(source_files, key))
                .flatten();
            match copied {
                Some(message) => {
                    text.push_str("# TODO translate\n");
                    text.push_str(&message);
                }
                None => {
                    if parsed.locale != source {
                        text.push_str("# TODO translate\n");
                    }
                    text.push_str(&format!("{key} = {key}\n"));
                }
            }
        }
        out.push(PlannedFile {
            locale: parsed.locale.clone(),
            path: format!("{localization_dir}/{}/{GATHERED_FILE}", parsed.locale),
            added: missing.into_iter().cloned().collect(),
            text,
        });
    }
    out
}

/// Writes a planned file under `root` atomically (a uniquely named temp file, then a rename).
///
/// # Errors
///
/// The directory cannot be created or the write or rename failed.
pub fn write_planned(root: &Path, file: &PlannedFile) -> std::io::Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = root.join(&file.path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("ftl.{}-{seq}.tmp", std::process::id()));
    if let Err(error) = std::fs::write(&tmp, file.text.as_bytes()).and_then(|()| std::fs::rename(&tmp, &path))
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ron_forms_are_found_with_lines() {
        let text = "(\n  text: (spans: [Loc(\"menu-start\")]),\n  label: \"loc:menu-quit\",\n  other: \"plain\",\n  bad: Loc(\"has space\"),\n)\n";
        let found = scan_ron(text);
        assert_eq!(
            found,
            vec![
                ("menu-start".to_owned(), 2),
                ("menu-quit".to_owned(), 3),
                ("has space".to_owned(), 5)
            ]
        );
    }

    #[test]
    fn rust_macro_calls_are_found_and_comments_skipped() {
        let text = "fn f() {\n    let a = loc!(\"hud-ammo\");\n    // loc!(\"commented\")\n    let b = regia_ui::loc!( \"items\", count = 3);\n}\n";
        assert_eq!(
            scan_rust(text),
            vec![("hud-ammo".to_owned(), 2), ("items".to_owned(), 4)]
        );
    }

    #[test]
    fn block_comments_hide_keys_but_strings_and_line_comments_do_not_open_them() {
        // `/* Loc("x") */` was gathered as a phantom key (ADR-1660).
        let ron = "(\n  /* Loc(\"ghost\") /* nested */ Loc(\"ghost2\")\n  */\n  a: Loc(\"real\"),\n  b: \"/*\", c: Loc(\"after-string\"),\n  // /* not a block\n  d: Loc(\"after-line-comment\"),\n)\n";
        assert_eq!(
            scan_ron(ron),
            vec![
                ("real".to_owned(), 4),
                ("after-string".to_owned(), 5),
                ("after-line-comment".to_owned(), 7)
            ]
        );
        let rust = "let q = '\"'; /* loc!(\"ghost\") */ let k = loc!(\"kept\");\n";
        assert_eq!(scan_rust(rust), vec![("kept".to_owned(), 1)]);
    }

    #[test]
    fn rust_macro_calls_allow_any_spacing_and_a_new_line_before_the_key() {
        let text = "a(loc!  (\"two-spaces\"));\nb(loc!\t(\"tab\"));\nlet c = loc!(\n    \"next-line\",\n);\n";
        assert_eq!(
            scan_rust(text),
            vec![
                ("two-spaces".to_owned(), 1),
                ("tab".to_owned(), 2),
                ("next-line".to_owned(), 3)
            ]
        );
        assert!(scan_rust("loc!(key)").is_empty(), "not a string literal");
    }

    #[test]
    fn raw_strings_hide_their_content_without_opening_comments() {
        // Regression: the scanner treated `r#"..."#` as a normal string, closed it at the
        // first inner quote and let a `/*` inside the raw string swallow the rest of the
        // file (real keys were dropped); a `loc!` inside a raw string was also gathered as
        // a phantom key.
        let text = "let s = r#\" \" /* \"#;\nlet k = loc!(\"menu\");\nlet t = r#\"loc!(\"ghost\")\"#;\nlet u = r##\"raw \"# inner\"##;\nlet v = loc!(\"kept\");\n";
        assert_eq!(
            scan_rust(text),
            vec![("menu".to_owned(), 2), ("kept".to_owned(), 5)]
        );
    }

    /// Corr-ui-b: the raw-string skip lived behind the "has a `/*`" early return, so a file with
    /// a raw string and no block comment still gathered the `loc!` lookalike inside it.
    #[test]
    fn raw_string_lookalikes_are_skipped_without_a_block_comment() {
        let text = "let t = r#\"loc!(\"ghost\")\"#;\nlet v = loc!(\"kept\");\n";
        assert_eq!(scan_rust(text), vec![("kept".to_owned(), 2)]);
    }

    /// The `r` that ends a word before a quote (here `four"`, reached after an unterminated
    /// string) is not a raw-string prefix: blanking from it dropped the keys after it.
    #[test]
    fn an_r_ending_a_word_does_not_open_a_raw_string() {
        let ron = "(a: Loc(\"one\"))\n(c: Loc(\"open\n(d: Loc(\"four\"))\n";
        assert_eq!(scan_ron(ron), vec![("one".to_owned(), 1), ("four".to_owned(), 3)]);
        // `br#"..."#` still opens one.
        let rust = "let t = br#\"loc!(\"ghost\")\"#;\nlet v = loc!(\"kept\");\n";
        assert_eq!(scan_rust(rust), vec![("kept".to_owned(), 2)]);
    }

    #[test]
    fn message_ids() {
        assert!(is_message_id("menu-start"));
        assert!(is_message_id("a_1"));
        assert!(!is_message_id("1a"));
        assert!(!is_message_id("-term"));
        assert!(!is_message_id("has space"));
        assert!(!is_message_id(""));
    }
}
