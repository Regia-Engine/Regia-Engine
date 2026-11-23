// SPDX-License-Identifier: MPL-2.0
//! `agent-coverage`: every method the agent dispatcher registers is documented, discoverable
//! and called by a test.
//!
//! For each `"area.name"` arm of `route` in `crates/regia_agent/src/dispatch.rs` the check
//! requires:
//!
//! 1. a row in [`regia_agent::schema::METHODS`] (and every schema method is dispatched),
//! 2. a bundled `agent.guide` topic whose text names the method ([`regia_agent::guide_mentions`]),
//! 3. a `book/COVERAGE.md` row whose page exists and names the method as a whole identifier,
//! 4. at least one Rust test that contains the method name as a string literal,
//! 5. a [`regia_agent::contract::CONTRACTS`] row with an example (ADR-1034), whose lowercase
//!    error codes each appear on one of the method's `book/COVERAGE.md` pages and whose `F`
//!    codes (plus the universal ones) each appear on `book/agents/protocol.md`,
//! 6. every parameter of its [`regia_agent::schema::METHODS`] row named, as a whole identifier, on
//!    one of the method's `book/COVERAGE.md` pages (a page may point to another for the details,
//!    but it says the parameter exists).
//!
//! A test counts only when that literal sits in test code: a file under a `tests/` directory,
//! a file whose first inner attribute is `#![cfg(test)]`, the item attached to a `#[cfg(test)]`
//! attribute, or a file pulled in by `#[cfg(test)] mod name;`. Looping over `schema::METHODS`
//! does not count — those names live in the schema table, not in the test.
//!
//! An extension in the workspace (`extensions/*`, `samples/*`) whose manifest requests
//! `agent_method` must ship a guide page in its own `book/` (spec 57 § 5.4): its methods exist only
//! at run time, so the manifest is what this check can see.
//!
//! Editor `editor.*` methods are host [`regia_agent::extension::AgentExtension`] handlers.
//! They are not `route` arms and they are not in the runtime schema, so this check ignores them.
//! The built-in extension methods (`regia_agent::builtin_extension_methods`: curve tables,
//! choosers, abilities, cloth; ADR-1809) are checked like routed ones, with the extension's schema
//! row and a contract in `regia_agent::contract::EXTENSION_CONTRACTS`.
//!
//! `book-check` runs this when `dispatch.rs` is present, which is how the merge gate picks it
//! up. `cargo xtask agent-coverage` runs it on its own. A book fixture with no dispatcher file
//! skips it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// What [`check`] found.
pub struct Report {
    /// One line per gap. Empty when the surface is covered.
    pub errors: Vec<String>,
    /// How many methods the dispatcher registers, plus the built-in extension methods.
    pub methods: usize,
}

/// Checks the workspace rooted at `root`.
pub fn check(root: &Path) -> Result<Report> {
    let dispatch_path = root.join("crates/regia_agent/src/dispatch.rs");
    let source = std::fs::read_to_string(&dispatch_path)
        .with_context(|| format!("reading {}", dispatch_path.display()))?;
    let mut routed = routed_methods(&source).map_err(anyhow::Error::msg)?;

    let mut schema: BTreeSet<String> = regia_agent::schema::METHODS
        .iter()
        .map(|method| method.name.to_string())
        .collect();
    // The built-in extension methods `AgentPlugin` installs (ADR-1809) have the same duties as
    // routed ones: their schema row is the extension's, their contract is in
    // `EXTENSION_CONTRACTS`.
    let mut builtin_errors = Vec::new();
    for method in regia_agent::builtin_extension_methods() {
        if routed.iter().any(|m| m == method) || schema.contains(method) {
            builtin_errors.push(format!(
                "built-in extension method `{method}` is also a native method"
            ));
            continue;
        }
        routed.push(method.to_string());
        schema.insert(method.to_string());
    }

    let coverage_path = root.join("book/COVERAGE.md");
    let coverage_text = std::fs::read_to_string(&coverage_path)
        .with_context(|| format!("reading {}", coverage_path.display()))?;
    let (rows, mut errors) = crate::book::coverage_table(&coverage_text);
    let book = root.join("book");
    let mut pages = BTreeMap::new();
    for row in &rows {
        let page = normalize_page(&row.page);
        if pages.contains_key(&page) {
            continue;
        }
        let path = book.join(&page);
        if path.is_file() {
            let text =
                std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            pages.insert(page, text);
        }
    }

    let tested = test_literals(root)?;
    let row_pairs: Vec<(String, String)> = rows.into_iter().map(|row| (row.method, row.page)).collect();
    let protocol_path = book.join("agents/protocol.md");
    let protocol_page = std::fs::read_to_string(&protocol_path)
        .with_context(|| format!("reading {}", protocol_path.display()))?;
    let contracts: Vec<(String, Vec<String>)> = regia_agent::contract::CONTRACTS
        .iter()
        .chain(regia_agent::contract::EXTENSION_CONTRACTS)
        .filter(|contract| !contract.example.trim().is_empty())
        .map(|contract| {
            (
                contract.method.to_string(),
                contract.errors.iter().map(|code| (*code).to_string()).collect(),
            )
        })
        .collect();
    let universal: Vec<String> = regia_agent::contract::UNIVERSAL_ERRORS
        .iter()
        .map(|code| (*code).to_string())
        .collect();
    errors.extend(contract_gaps(
        &contracts,
        &universal,
        &schema,
        &row_pairs,
        &pages,
        &protocol_page,
    ));
    errors.extend(coverage_gaps(
        &routed,
        &schema,
        &|method| regia_agent::guide_mentions(method),
        &row_pairs,
        &pages,
        &tested,
    ));
    let params: Vec<(String, Vec<String>)> = regia_agent::schema::METHODS
        .iter()
        .map(|method| {
            (
                method.name.to_string(),
                method.params.iter().map(|param| param.name.to_string()).collect(),
            )
        })
        .collect();
    errors.extend(param_gaps(&params, &row_pairs, &pages));
    errors.extend(builtin_errors);
    errors.extend(extension_gaps(root));
    errors.sort();
    errors.dedup();
    let methods = routed.len();
    Ok(Report { errors, methods })
}

/// One error per workspace extension that registers agent methods and has no `book/*.md`.
fn extension_gaps(root: &Path) -> Vec<String> {
    let mut errors = Vec::new();
    for parent in ["extensions", "samples"] {
        let Ok(entries) = std::fs::read_dir(root.join(parent)) else {
            continue;
        };
        for dir in entries.filter_map(Result::ok).map(|entry| entry.path()) {
            let Ok(text) = std::fs::read_to_string(dir.join("regia-plugin.toml")) else {
                continue;
            };
            let requests_methods = text
                .parse::<toml::Table>()
                .ok()
                .and_then(|table| table.get("capabilities")?.as_array().cloned())
                .is_some_and(|list| list.iter().any(|c| c.as_str() == Some("agent_method")));
            let has_page = std::fs::read_dir(dir.join("book")).is_ok_and(|pages| {
                pages
                    .filter_map(Result::ok)
                    .any(|page| page.path().extension().is_some_and(|e| e == "md"))
            });
            if requests_methods && !has_page {
                errors.push(format!(
                    "extension {}: requests agent_method but has no guide page in book/",
                    dir.display()
                ));
            }
        }
    }
    errors
}

/// Prints the report and returns the process exit code.
pub fn run(root: &Path) -> Result<std::process::ExitCode> {
    let report = check(root)?;
    if report.errors.is_empty() {
        println!("agent-coverage: ok ({} methods)", report.methods);
        Ok(std::process::ExitCode::SUCCESS)
    } else {
        for error in &report.errors {
            eprintln!("error: {error}");
        }
        eprintln!("agent-coverage: {} problem(s)", report.errors.len());
        Ok(std::process::ExitCode::FAILURE)
    }
}

/// `"area.name"` arms of `fn route`'s `match method`, in source order.
fn routed_methods(source: &str) -> Result<Vec<String>, String> {
    let fn_at = source
        .find("fn route(")
        .ok_or_else(|| "dispatch.rs: no `fn route(`".to_string())?;
    let match_rel = source[fn_at..]
        .find("match method")
        .ok_or_else(|| "dispatch.rs: `fn route` has no `match method`".to_string())?;
    let match_at = fn_at + match_rel;
    let brace = source[match_at..]
        .find('{')
        .ok_or_else(|| "dispatch.rs: `match method` has no body".to_string())?;
    let body_start = match_at + brace + 1;
    let body_end = match_end(source, body_start)
        .ok_or_else(|| "dispatch.rs: `match method` body is not closed".to_string())?;
    let body = &source[body_start..body_end];

    let mut methods = Vec::new();
    let mut seen = BTreeSet::new();
    let mut i = 0;
    while i < body.len() {
        if let Some(end) = skip_comment(body, i) {
            i = end;
            continue;
        }
        if let Some((end, literal)) = take_string(body, i) {
            let after = skip_ws_and_comments(body, end);
            if body[after..].starts_with("=>") {
                if !is_method_name(&literal) {
                    return Err(format!("dispatch.rs: arm `{literal}` is not an area.name method"));
                }
                if !seen.insert(literal.clone()) {
                    return Err(format!("dispatch.rs: duplicate arm `{literal}`"));
                }
                methods.push(literal);
            }
            i = end;
            continue;
        }
        i += utf8_len(body, i);
    }
    if methods.is_empty() {
        return Err("dispatch.rs: no methods registered in `route`".into());
    }
    Ok(methods)
}

fn coverage_gaps(
    routed: &[String],
    schema: &BTreeSet<String>,
    guide: &dyn Fn(&str) -> bool,
    rows: &[(String, String)],
    pages: &BTreeMap<String, String>,
    tested: &BTreeSet<String>,
) -> Vec<String> {
    let routed_set: BTreeSet<&str> = routed.iter().map(String::as_str).collect();
    let mut errors = Vec::new();
    for method in routed {
        if !schema.contains(method) {
            errors.push(format!("dispatched method `{method}` has no schema entry"));
        }
        if !guide(method) {
            errors.push(format!(
                "dispatched method `{method}` is not named by any agent.guide topic"
            ));
        }
        let method_rows: Vec<_> = rows.iter().filter(|(name, _)| name == method).collect();
        if method_rows.is_empty() {
            errors.push(format!(
                "dispatched method `{method}` has no row in book/COVERAGE.md"
            ));
        }
        for (_, page) in method_rows {
            let page = normalize_page(page);
            match pages.get(&page) {
                None => errors.push(format!(
                    "dispatched method `{method}` maps to `{page}`, which does not exist"
                )),
                Some(text) if !regia_agent::mentions_token(text, method) => errors.push(format!(
                    "dispatched method `{method}` is mapped to `{page}` but that page does not mention it"
                )),
                Some(_) => {}
            }
        }
        if !tested.contains(method) {
            errors.push(format!(
                "dispatched method `{method}` is not called by any test (no string literal in test code)"
            ));
        }
    }
    for method in schema {
        if !routed_set.contains(method.as_str()) {
            errors.push(format!("schema method `{method}` is not dispatched by `route`"));
        }
    }
    for (method, page) in rows {
        if !routed_set.contains(method.as_str()) && !schema.contains(method) {
            errors.push(format!(
                "book/COVERAGE.md maps `{method}` to `{page}`, but `{method}` is not dispatched"
            ));
        }
    }
    errors
}

/// Documentation gaps in the method contracts: a schema method without a contract (or with an
/// empty example), a lowercase error code that none of the method's book pages names, and an
/// `F` code (a contract's or a universal one) that the protocol page's error table lacks.
fn contract_gaps(
    contracts: &[(String, Vec<String>)],
    universal: &[String],
    schema: &BTreeSet<String>,
    rows: &[(String, String)],
    pages: &BTreeMap<String, String>,
    protocol_page: &str,
) -> Vec<String> {
    let mut errors = Vec::new();
    for method in schema {
        if !contracts.iter().any(|(name, _)| name == method) {
            errors.push(format!(
                "schema method `{method}` has no contract with an example in regia_agent::contract::CONTRACTS"
            ));
        }
    }
    let mut f_codes: BTreeSet<&str> = universal.iter().map(String::as_str).collect();
    for (method, codes) in contracts {
        for code in codes {
            if code.starts_with('F') {
                f_codes.insert(code.as_str());
                continue;
            }
            let documented = rows
                .iter()
                .filter(|(name, _)| name == method)
                .filter_map(|(_, page)| pages.get(&normalize_page(page)))
                .any(|text| regia_agent::mentions_token(text, code));
            if !documented {
                errors.push(format!(
                    "`{method}` can answer error code `{code}`, but none of its book/COVERAGE.md pages names it"
                ));
            }
        }
    }
    for code in f_codes {
        if !regia_agent::mentions_token(protocol_page, code) {
            errors.push(format!(
                "error code `{code}` is not in the error table of book/agents/protocol.md"
            ));
        }
    }
    errors
}

/// Parameter documentation gaps: a schema parameter that none of the method's `book/COVERAGE.md`
/// pages names as a whole identifier. A method with no row (or no existing page) is skipped here;
/// `coverage_gaps` reports it.
fn param_gaps(
    params: &[(String, Vec<String>)],
    rows: &[(String, String)],
    pages: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for (method, names) in params {
        let texts: Vec<&String> = rows
            .iter()
            .filter(|(name, _)| name == method)
            .filter_map(|(_, page)| pages.get(&normalize_page(page)))
            .collect();
        if texts.is_empty() {
            continue;
        }
        for name in names {
            if !texts.iter().any(|text| regia_agent::mentions_token(text, name)) {
                errors.push(format!(
                    "`{method}` takes parameter `{name}`, but none of its book/COVERAGE.md pages names it"
                ));
            }
        }
    }
    errors
}

pub(crate) fn normalize_page(page: &str) -> String {
    page.trim().trim_start_matches("./").replace('\\', "/")
}

fn is_method_name(name: &str) -> bool {
    let Some((area, method)) = name.split_once('.') else {
        return false;
    };
    let ident = |part: &str| {
        let mut chars = part.chars();
        matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    !method.contains('.') && ident(area) && ident(method)
}

/// String literals that appear in test code under `root`.
pub(crate) fn test_literals(root: &Path) -> Result<BTreeSet<String>> {
    let mut files = Vec::new();
    walk_rs(root, &mut files)?;
    let mut literals = BTreeSet::new();
    let mut extra: Vec<PathBuf> = Vec::new();
    let mut seen_extra = BTreeSet::new();
    for path in &files {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        if is_whole_test_file(root, path, &text) {
            literals.extend(string_literals(&text));
            extra.extend(external_mods(path, &text));
        } else {
            for (start, end) in test_spans(&text) {
                let slice = &text[start..end];
                literals.extend(string_literals(slice));
                if let Some(name) = external_mod_name(slice) {
                    extra.extend(resolve_mod(path, name));
                }
            }
        }
    }
    while let Some(path) = extra.pop() {
        if !path.is_file() || !seen_extra.insert(path.clone()) {
            continue;
        }
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        literals.extend(string_literals(&text));
        extra.extend(external_mods(&path, &text));
    }
    Ok(literals)
}

pub(crate) fn walk_rs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            // Dot directories hold no workspace code, and the main checkout's `.claude/worktrees/`
            // holds other lanes' whole trees: their tests must not count for this one.
            if name == "target" || name.starts_with('.') {
                continue;
            }
            walk_rs(&path, out)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// A file under a `tests` directory of the tree (relative to `root`, so a checkout that itself
/// lives under some `.../tests/...` path is not all test code), or one whose first inner
/// attribute is `#![cfg(test)]`.
pub(crate) fn is_whole_test_file(root: &Path, path: &Path, text: &str) -> bool {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .any(|component| component.as_os_str() == "tests")
        || is_inner_cfg_test(text)
}

fn is_inner_cfg_test(text: &str) -> bool {
    let mut i = 0;
    loop {
        i = skip_ws_and_comments(text, i);
        if i >= text.len() {
            return false;
        }
        // Inner doc comments are comments; `skip_ws_and_comments` already passed them.
        if text[i..].starts_with("#![") {
            let Some(end) = attr_end(text, i) else {
                return false;
            };
            if is_cfg_test(&attr_body(text, i)) {
                return true;
            }
            i = end;
            continue;
        }
        return false;
    }
}

/// Byte ranges of items gated by `#[cfg(test)]`.
pub(crate) fn test_spans(text: &str) -> Vec<(usize, usize)> {
    if is_inner_cfg_test(text) {
        return vec![(0, text.len())];
    }
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(end) = skip_comment(text, i) {
            i = end;
            continue;
        }
        if let Some((end, _)) = take_string(text, i) {
            i = end;
            continue;
        }
        if text[i..].starts_with("#[") {
            let Some(end) = attr_end(text, i) else {
                i += 1;
                continue;
            };
            if is_cfg_test(&attr_body(text, i)) {
                let item_end = item_end(text, end);
                spans.push((i, item_end));
                i = item_end;
                continue;
            }
            i = end;
            continue;
        }
        i += utf8_len(text, i);
    }
    spans
}

fn item_end(text: &str, mut i: usize) -> usize {
    loop {
        i = skip_ws_and_comments(text, i);
        if i >= text.len() {
            return text.len();
        }
        if text[i..].starts_with("#[") || text[i..].starts_with("#![") {
            let Some(end) = attr_end(text, i) else {
                return text.len();
            };
            i = end;
            continue;
        }
        break;
    }
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut started = false;
    while i < bytes.len() {
        if let Some(end) = skip_comment(text, i) {
            i = end;
            continue;
        }
        if let Some((end, _)) = take_string(text, i) {
            i = end;
            continue;
        }
        match bytes[i] {
            b'{' => {
                depth += 1;
                started = true;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                i += 1;
                if started && depth == 0 {
                    return i;
                }
            }
            b';' if depth == 0 => return i + 1,
            _ => i += utf8_len(text, i),
        }
    }
    text.len()
}

/// The name of the out-of-line module `item` declares (`#[cfg(test)] mod tests;`), after its
/// attributes and an optional visibility (`pub`, `pub(crate)`, `pub(super)`, `pub(in path)`);
/// `None` for an inline module or another item.
pub(crate) fn external_mod_name(item: &str) -> Option<&str> {
    let mut i = skip_ws_and_comments(item, 0);
    while item[i..].starts_with("#[") || item[i..].starts_with("#![") {
        i = attr_end(item, i)?;
        i = skip_ws_and_comments(item, i);
    }
    if is_keyword(item, i, "pub") {
        i = skip_ws_and_comments(item, i + "pub".len());
        if item[i..].starts_with('(') {
            // `pub(crate)`, `pub(super)`, `pub(self)`, `pub(in a::b)`: no nested parentheses.
            i += item[i..].find(')')? + 1;
            i = skip_ws_and_comments(item, i);
        }
    }
    if !is_keyword(item, i, "mod") {
        return None;
    }
    i = skip_ws_and_comments(item, i + "mod".len());
    let (name, next) = read_ident(item, i)?;
    let next = skip_ws_and_comments(item, next);
    if item[next..].starts_with(';') {
        Some(name)
    } else {
        None
    }
}

pub(crate) fn external_mods(parent: &Path, text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if let Some(end) = skip_comment(text, i) {
            i = end;
            continue;
        }
        if let Some((end, _)) = take_string(text, i) {
            i = end;
            continue;
        }
        if is_keyword(text, i, "mod")
            && let Some(name) = external_mod_name(&text[i..])
        {
            out.extend(resolve_mod(parent, name));
        }
        i += utf8_len(text, i);
    }
    out
}

pub(crate) fn resolve_mod(parent: &Path, name: &str) -> Vec<PathBuf> {
    let stem = parent.file_stem().and_then(|stem| stem.to_str());
    let dir = if matches!(stem, Some("mod" | "lib" | "main")) {
        parent.parent().map(Path::to_path_buf)
    } else {
        Some(parent.with_extension(""))
    };
    let Some(dir) = dir else {
        return Vec::new();
    };
    [dir.join(format!("{name}.rs")), dir.join(name).join("mod.rs")]
        .into_iter()
        .filter(|path| path.is_file())
        .collect()
}

pub(crate) fn is_cfg_test(body: &str) -> bool {
    body.split_whitespace().collect::<String>() == "cfg(test)"
}

pub(crate) fn attr_body(text: &str, hash: usize) -> String {
    let bytes = text.as_bytes();
    let mut i = hash + 1;
    if bytes.get(i) == Some(&b'!') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'[') {
        return String::new();
    }
    i += 1;
    let start = i;
    let mut depth = 1i32;
    while i < bytes.len() {
        match bytes[i] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return text[start..i].to_string();
                }
            }
            _ => {}
        }
        i += 1;
    }
    String::new()
}

pub(crate) fn attr_end(text: &str, hash: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = hash + 1;
    if bytes.get(i) == Some(&b'!') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    i += 1;
    let mut depth = 1i32;
    while i < bytes.len() {
        match bytes[i] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

pub(crate) fn match_end(text: &str, mut i: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 1i32;
    while i < bytes.len() {
        if let Some(end) = skip_comment(text, i) {
            i = end;
            continue;
        }
        if let Some((end, _)) = take_string(text, i) {
            i = end;
            continue;
        }
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += utf8_len(text, i);
    }
    None
}

fn string_literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if let Some(end) = skip_comment(text, i) {
            i = end;
            continue;
        }
        if let Some(end) = skip_char_or_lifetime(text, i) {
            i = end;
            continue;
        }
        if let Some((end, literal)) = take_string(text, i) {
            out.push(literal);
            i = end;
            continue;
        }
        i += utf8_len(text, i);
    }
    out
}

pub(crate) fn take_string(text: &str, i: usize) -> Option<(usize, String)> {
    if let Some(found) = take_raw_string(text, i) {
        return Some(found);
    }
    take_cooked_string(text, i)
}

fn take_raw_string(text: &str, i: usize) -> Option<(usize, String)> {
    let bytes = text.as_bytes();
    if i > 0 && is_ident_byte(bytes[i - 1]) {
        return None;
    }
    let mut j = i;
    if matches!(bytes.get(j), Some(b'b' | b'c')) {
        j += 1;
    }
    if bytes.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let mut hashes = 0usize;
    while bytes.get(j) == Some(&b'#') {
        hashes += 1;
        j += 1;
    }
    if bytes.get(j) != Some(&b'"') {
        return None;
    }
    j += 1;
    let start = j;
    let closer = format!("\"{}", "#".repeat(hashes));
    let rel = text[j..].find(&closer)?;
    Some((j + rel + closer.len(), text[start..j + rel].to_string()))
}

fn take_cooked_string(text: &str, i: usize) -> Option<(usize, String)> {
    let bytes = text.as_bytes();
    if i > 0 && is_ident_byte(bytes[i - 1]) {
        return None;
    }
    let mut j = i;
    if matches!(bytes.get(j), Some(b'b' | b'c')) {
        j += 1;
        if j > 1 && is_ident_byte(bytes[j - 2]) {
            return None;
        }
    }
    if bytes.get(j) != Some(&b'"') {
        return None;
    }
    j += 1;
    let mut out = String::new();
    while j < bytes.len() {
        if bytes[j] == b'\\' {
            let next = *bytes.get(j + 1)?;
            out.push(match next {
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                b'\\' => '\\',
                b'\'' => '\'',
                b'"' => '"',
                b'0' => '\0',
                other => other as char,
            });
            j += 2;
            continue;
        }
        if bytes[j] == b'"' {
            return Some((j + 1, out));
        }
        let ch = text[j..].chars().next()?;
        out.push(ch);
        j += ch.len_utf8();
    }
    None
}

pub(crate) fn skip_char_or_lifetime(text: &str, i: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(i) != Some(&b'\'') {
        return None;
    }
    if i > 0 && (is_ident_byte(bytes[i - 1]) || bytes[i - 1] == b'\'') {
        return None;
    }
    let next = *bytes.get(i + 1)?;
    if next == b'\\' {
        let mut j = i + 2;
        if bytes.get(j) == Some(&b'u') {
            while j < bytes.len() && bytes[j] != b'}' {
                j += 1;
            }
            j += 1;
        } else {
            j += 1;
        }
        if bytes.get(j) == Some(&b'\'') {
            return Some(j + 1);
        }
        return None;
    }
    if is_ident_byte(next) {
        let mut j = i + 1;
        while j < bytes.len() && is_ident_byte(bytes[j]) {
            j += 1;
        }
        if j == i + 2 && bytes.get(j) == Some(&b'\'') {
            return Some(j + 1);
        }
        return Some(j);
    }
    None
}

pub(crate) fn skip_comment(text: &str, i: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(i) != Some(&b'/') {
        return None;
    }
    match bytes.get(i + 1) {
        Some(b'/') => {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            Some(j)
        }
        Some(b'*') => {
            let mut j = i + 2;
            while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                j += 1;
            }
            if j + 1 < bytes.len() {
                Some(j + 2)
            } else {
                Some(bytes.len())
            }
        }
        _ => None,
    }
}

pub(crate) fn skip_ws_and_comments(text: &str, mut i: usize) -> usize {
    loop {
        let start = i;
        while i < text.len() && text.as_bytes()[i].is_ascii_whitespace() {
            i += 1;
        }
        if let Some(end) = skip_comment(text, i) {
            i = end;
        }
        if i == start {
            return i;
        }
    }
}

pub(crate) fn is_keyword(text: &str, i: usize, word: &str) -> bool {
    if !text[i..].starts_with(word) {
        return false;
    }
    let bytes = text.as_bytes();
    let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
    let after = i + word.len();
    let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
    before_ok && after_ok
}

pub(crate) fn read_ident(text: &str, i: usize) -> Option<(&str, usize)> {
    let bytes = text.as_bytes();
    if i >= bytes.len() || !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
        return None;
    }
    let mut j = i + 1;
    while j < bytes.len() && is_ident_byte(bytes[j]) {
        j += 1;
    }
    Some((&text[i..j], j))
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

pub(crate) fn utf8_len(text: &str, i: usize) -> usize {
    text[i..].chars().next().map_or(1, char::len_utf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_with_agent_methods_need_a_guide_page() {
        let root = std::env::temp_dir().join(format!("xtask-ext-gaps-{}", std::process::id()));
        let dir = root.join("extensions/talk");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join("regia-plugin.toml"),
            "capabilities = [\"agent_method\"]\n",
        )
        .expect("write");
        assert_eq!(extension_gaps(&root).len(), 1);
        std::fs::create_dir_all(dir.join("book")).expect("dir");
        std::fs::write(dir.join("book/talk.md"), "# Talk\n").expect("write");
        assert!(extension_gaps(&root).is_empty());
        std::fs::remove_file(dir.join("book/talk.md")).expect("remove");
        std::fs::write(dir.join("regia-plugin.toml"), "capabilities = [\"systems\"]\n").expect("write");
        assert!(extension_gaps(&root).is_empty(), "no methods, no page needed");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parses_route_arms_and_ignores_comments() {
        let src = r#"
            fn helper() { match method { "not.routed" => {} } }
            fn route(method: &str) {
                let note = "skip.me";
                match method {
                    // "skipped.one" =>
                    "agent.hello" => done(),
                    /* "skipped.two" => */ "scene.spawn" => done(),
                    "scene.spawn_primitive" => done("scene.spawn"),
                    other => unknown(other),
                }
            }
        "#;
        let methods = routed_methods(src).expect("parse");
        assert_eq!(
            methods,
            vec!["agent.hello", "scene.spawn", "scene.spawn_primitive"]
        );
    }

    #[test]
    fn duplicate_arms_are_an_error() {
        let src = r#"
            fn route() {
                match method {
                    "agent.hello" => {}
                    "agent.hello" => {}
                }
            }
        "#;
        let err = routed_methods(src).expect_err("duplicate");
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn string_literals_skip_comments_and_keep_exact_text() {
        let src = r##"
            // "scene.spawn"
            /* "scene.despawn" */
            let a = "scene.spawn";
            let b = r#"scene.duplicate"#;
            let c = "scene.spawn_primitive";
            let d = "say \"scene.reparent\"";
            let e = 'x';
            let f = b"assets.list";
        "##;
        let literals = string_literals(src);
        assert!(literals.contains(&"scene.spawn".to_string()));
        assert!(literals.contains(&"scene.duplicate".to_string()));
        assert!(literals.contains(&"scene.spawn_primitive".to_string()));
        assert!(literals.contains(&"say \"scene.reparent\"".to_string()));
        assert!(literals.contains(&"assets.list".to_string()));
        assert!(!literals.iter().any(|lit| lit == "scene.despawn"));
    }

    #[test]
    fn gaps_name_each_missing_piece() {
        let routed = vec!["scene.spawn".to_string(), "scene.extra".to_string()];
        let schema = BTreeSet::from(["scene.spawn".to_string()]);
        let rows = vec![("scene.spawn".to_string(), "agents/scene.md".to_string())];
        let mut pages = BTreeMap::new();
        pages.insert(
            "agents/scene.md".to_string(),
            "Use scene.spawn_primitive.".to_string(),
        );
        let tested = BTreeSet::new();
        let guide = |method: &str| method == "scene.spawn";
        let errors = coverage_gaps(&routed, &schema, &guide, &rows, &pages, &tested);
        let text = errors.join("\n");
        assert!(
            text.contains("scene.extra") && text.contains("no schema"),
            "{text}"
        );
        assert!(text.contains("not named by any agent.guide"), "{text}");
        assert!(
            text.contains("does not mention it"),
            "word boundary, not a prefix: {text}"
        );
        assert!(text.contains("not called by any test"), "{text}");
    }

    #[test]
    fn a_covered_method_has_no_gaps() {
        let routed = vec!["scene.spawn".to_string()];
        let schema = BTreeSet::from(["scene.spawn".to_string()]);
        let rows = vec![("scene.spawn".to_string(), "agents/scene.md".to_string())];
        let mut pages = BTreeMap::new();
        pages.insert(
            "agents/scene.md".to_string(),
            "Call scene.spawn, then scene.spawn_primitive.".to_string(),
        );
        let tested = BTreeSet::from(["scene.spawn".to_string()]);
        let errors = coverage_gaps(
            &routed,
            &schema,
            &|method| method == "scene.spawn",
            &rows,
            &pages,
            &tested,
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn contract_gaps_name_missing_contracts_and_undocumented_codes() {
        let schema = BTreeSet::from(["ui.list".to_string(), "ui.tween".to_string()]);
        let contracts = vec![(
            "ui.list".to_string(),
            vec![
                "F0003".to_string(),
                "not_a_list".to_string(),
                "no_selection".to_string(),
            ],
        )];
        let rows = vec![("ui.list".to_string(), "agents/ui-list.md".to_string())];
        let mut pages = BTreeMap::new();
        pages.insert(
            "agents/ui-list.md".to_string(),
            "ui.list answers `not_a_list` for other nodes.".to_string(),
        );
        let universal = vec!["F0002".to_string()];
        let errors = contract_gaps(
            &contracts,
            &universal,
            &schema,
            &rows,
            &pages,
            "| F0002 | -32602 | Missing or wrong parameters |",
        );
        let text = errors.join("\n");
        assert_eq!(errors.len(), 3, "{text}");
        assert!(text.contains("`ui.tween` has no contract"), "{text}");
        assert!(text.contains("`no_selection`"), "{text}");
        assert!(!text.contains("`not_a_list`"), "{text}");
        assert!(text.contains("`F0003` is not in the error table"), "{text}");
    }

    #[test]
    fn param_gaps_name_each_undocumented_parameter() {
        let params = vec![
            (
                "stream.pin".to_string(),
                vec!["center".to_string(), "ttl_steps".to_string()],
            ),
            ("stream.trace".to_string(), vec!["limit".to_string()]),
            ("stream.unmapped".to_string(), vec!["anything".to_string()]),
        ];
        let rows = vec![
            ("stream.pin".to_string(), "agents/a.md".to_string()),
            ("stream.pin".to_string(), "./agents/b.md".to_string()),
            ("stream.trace".to_string(), "agents/a.md".to_string()),
        ];
        let pages = BTreeMap::from([
            // `ttl_steps_max` is not `ttl_steps`: whole identifiers only.
            (
                "agents/a.md".to_string(),
                "`stream.pin { center }` and `stream.trace`; ttl_steps_max".to_string(),
            ),
            ("agents/b.md".to_string(), "`stream.pin` again".to_string()),
        ]);
        let errors = param_gaps(&params, &rows, &pages);
        assert_eq!(
            errors,
            vec![
                "`stream.pin` takes parameter `ttl_steps`, but none of its book/COVERAGE.md pages names it",
                "`stream.trace` takes parameter `limit`, but none of its book/COVERAGE.md pages names it",
            ],
            "a method without a row is coverage_gaps' error, not this one"
        );
        let mut documented = pages.clone();
        // A parameter named on any one of the method's pages is documented; a page the method is
        // not mapped to does not count (`limit` on b.md does not document `stream.trace`).
        documented.insert(
            "agents/b.md".to_string(),
            "`stream.pin` ttl_steps; `stream.trace` limit".to_string(),
        );
        assert_eq!(
            param_gaps(&params, &rows, &documented),
            vec!["`stream.trace` takes parameter `limit`, but none of its book/COVERAGE.md pages names it"]
        );
        documented.insert(
            "agents/a.md".to_string(),
            "`stream.pin { center }` `stream.trace { limit }`".to_string(),
        );
        assert!(param_gaps(&params, &rows, &documented).is_empty());
    }

    #[test]
    fn test_literals_ignore_production_strings() {
        let dir = std::env::temp_dir().join(format!(
            "regia-agent-coverage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("tests_mod")).expect("mkdir");
        std::fs::write(
            src.join("lib.rs"),
            r#"
            const PROD: &str = "scene.spawn";
            #[cfg(test)]
            mod coverage_calls;
            #[cfg(test)]
            mod tests {
                const INSIDE: &str = "scene.despawn";
            }
            const AFTER: &str = "scene.reparent";
            "#,
        )
        .expect("write");
        std::fs::write(
            src.join("coverage_calls.rs"),
            "const CALLED: &str = \"scene.duplicate\";\n",
        )
        .expect("write");
        std::fs::create_dir_all(dir.join("tests")).expect("mkdir");
        std::fs::write(
            dir.join("tests/call.rs"),
            "fn call() { let _ = \"scene.serialize\"; }\n",
        )
        .expect("write");
        std::fs::write(src.join("other.rs"), "const ONLY: &str = \"assets.list\";\n").expect("write");

        // Another lane's worktree inside this checkout: its tests are not this tree's tests.
        let sibling = dir.join(".claude/worktrees/other-lane/tests");
        std::fs::create_dir_all(&sibling).expect("mkdir");
        std::fs::write(
            sibling.join("call.rs"),
            "fn call() { let _ = \"assets.load\"; }\n",
        )
        .expect("write");

        let literals = test_literals(&dir).expect("scan");
        assert!(!literals.contains("assets.load"), "{literals:?}");
        assert!(literals.contains("scene.despawn"), "{literals:?}");
        assert!(literals.contains("scene.duplicate"), "{literals:?}");
        assert!(literals.contains("scene.serialize"), "{literals:?}");
        assert!(!literals.contains("scene.spawn"), "{literals:?}");
        assert!(!literals.contains("scene.reparent"), "{literals:?}");
        assert!(!literals.contains("assets.list"), "{literals:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A test module declared with a visibility (`#[cfg(test)] pub(crate) mod tests;`, so other
    /// test modules can share its helpers) is still test code: the editor's `os_drop/tests.rs`
    /// calls `editor.import_files` that way.
    #[test]
    fn external_test_modules_with_a_visibility_are_test_code() {
        for (item, name) in [
            ("#[cfg(test)]\nmod tests;", Some("tests")),
            ("#[cfg(test)]\npub(crate) mod tests;", Some("tests")),
            ("#[cfg(test)] pub mod helpers;", Some("helpers")),
            ("#[cfg(test)]\npub(super) mod calls ;", Some("calls")),
            ("#[cfg(test)]\npub(in crate::a) mod deep;", Some("deep")),
            ("#[cfg(test)]\npub(crate) mod inline { const X: u8 = 1; }", None),
            ("#[cfg(test)]\npub(crate) fn helper() {}", None),
            ("#[cfg(test)]\npub(crate) use super::modded;", None),
        ] {
            assert_eq!(external_mod_name(item), name, "{item}");
        }
        let dir = regia_test_utils::temp::unique_temp_dir("agent-coverage-pub-mod");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("drop")).expect("mkdir");
        std::fs::write(
            src.join("lib.rs"),
            "pub mod drop;\nconst PROD: &str = \"scene.spawn\";\n",
        )
        .expect("write");
        std::fs::write(
            src.join("drop.rs"),
            "pub fn f() {}\n#[cfg(test)]\npub(crate) mod tests;\n",
        )
        .expect("write");
        std::fs::write(
            src.join("drop/tests.rs"),
            "fn call() { let _ = \"editor.import_files\"; }\n",
        )
        .expect("write");
        let literals = test_literals(&dir).expect("scan");
        assert!(literals.contains("editor.import_files"), "{literals:?}");
        assert!(!literals.contains("scene.spawn"), "{literals:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_checkout_under_a_tests_directory_is_not_all_test_code() {
        let root = std::env::temp_dir().join(format!(
            "regia-agent-coverage-nested-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let checkout = root.join("tests").join("checkout");
        std::fs::create_dir_all(checkout.join("src")).expect("mkdir");
        std::fs::write(
            checkout.join("src/lib.rs"),
            "const PROD: &str = \"scene.spawn\";\n",
        )
        .expect("write");
        let literals = test_literals(&checkout).expect("scan");
        let _ = std::fs::remove_dir_all(&root);
        assert!(!literals.contains("scene.spawn"), "{literals:?}");
    }
}
