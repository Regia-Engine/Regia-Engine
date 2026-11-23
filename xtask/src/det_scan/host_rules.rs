// SPDX-License-Identifier: MPL-2.0
//! Original lexical checks for host-dependent operations (ADR-8367).
//!
//! Work on the existing comment/literal/test-blanked source, retaining token line numbers.
//! Balanced delimiters bound cfg predicates and sort arguments; this is not type or dataflow analysis.

use std::collections::BTreeMap;

type Matches = BTreeMap<usize, Vec<(&'static str, String)>>;

struct Token<'a> {
    text: &'a str,
    line: usize,
}

fn tokens(source: &str) -> Vec<Token<'_>> {
    let mut result = Vec::new();
    let mut chars = source.char_indices().peekable();
    let mut line = 1;
    while let Some((start, ch)) = chars.next() {
        if ch == '\n' {
            line += 1;
        }
        if ch.is_whitespace() {
            continue;
        }
        let mut end = start + ch.len_utf8();
        if ch.is_alphanumeric() || ch == '_' {
            while let Some(&(at, next)) = chars.peek() {
                if !(next.is_alphanumeric() || next == '_') {
                    break;
                }
                end = at + next.len_utf8();
                chars.next();
            }
        }
        result.push(Token {
            text: &source[start..end],
            line,
        });
    }
    result
}

fn group_ends(tokens: &[Token<'_>]) -> Vec<Option<usize>> {
    let mut ends = vec![None; tokens.len()];
    let mut stack = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        match token.text {
            "(" | "[" | "{" => stack.push(i),
            ")" | "]" | "}" => {
                if let Some(start) = stack.pop() {
                    ends[start] = Some(i);
                }
            }
            _ => {}
        }
    }
    ends
}

fn path_at(tokens: &[Token<'_>], start: usize, path: &str) -> bool {
    let mut at = start;
    for (i, part) in path.split("::").enumerate() {
        if i > 0 {
            if tokens.get(at).is_none_or(|t| t.text != ":")
                || tokens.get(at + 1).is_none_or(|t| t.text != ":")
            {
                return false;
            }
            at += 2;
        }
        if tokens.get(at).is_none_or(|t| t.text != part) {
            return false;
        }
        at += 1;
    }
    true
}

fn add(out: &mut Matches, line: usize, rule: &'static str, what: &str) {
    let findings = out.entry(line).or_default();
    if !findings.iter().any(|(r, _)| *r == rule) {
        findings.push((rule, what.to_owned()));
    }
}

pub(super) fn findings(source: &str) -> Matches {
    let tokens = tokens(source);
    let ends = group_ends(&tokens);
    let mut out = Matches::new();
    // Paths also match as function values and across whitespace/newlines. Identifier tokens keep
    // MyInstant and my_thread distinct from Instant and thread.
    let paths: &[(&str, &[&str])] = &[
        (
            "wall_clock",
            &[
                "Instant::now",
                "SystemTime::now",
                "OffsetDateTime::now",
                "OffsetDateTime::now_utc",
                "OffsetDateTime::now_local",
                "chrono",
            ],
        ),
        ("os_random", &["rand::random", "rand::rng", "fastrand"]),
        (
            "env_read",
            &[
                "env::var",
                "env::var_os",
                "env::vars",
                "env::vars_os",
                "env::args",
                "env::args_os",
                "env::current_dir",
                "env::current_exe",
            ],
        ),
        (
            "thread_spawn",
            &["thread::spawn", "thread::Builder", "tokio::spawn"],
        ),
        ("address_order", &["ptr::addr", "Arc::as_ptr"]),
    ];
    for (i, token) in tokens.iter().enumerate() {
        for &(rule, paths) in paths {
            if let Some(path) = paths.iter().find(|path| {
                path_at(&tokens, i, path)
                    && (path.contains("::")
                        || (tokens.get(i + 1).is_some_and(|t| t.text == ":")
                            && tokens.get(i + 2).is_some_and(|t| t.text == ":")))
            }) {
                add(&mut out, token.line, rule, path);
            }
        }
        match token.text {
            "UNIX_EPOCH" => add(&mut out, token.line, "wall_clock", token.text),
            "RandomState" | "thread_rng" | "getrandom" | "OsRng" => {
                add(&mut out, token.line, "os_random", token.text);
            }
            "is_x86_feature_detected" | "is_aarch64_feature_detected"
                if tokens.get(i + 1).is_some_and(|t| t.text == "!") =>
            {
                add(&mut out, token.line, "arch_cfg", token.text);
            }
            "cfg" | "cfg_attr" => {
                let open = i + 1 + usize::from(tokens.get(i + 1).is_some_and(|t| t.text == "!"));
                if tokens.get(open).is_some_and(|t| t.text == "(")
                    && let Some(end) = ends[open]
                    && tokens[open + 1..end]
                        .iter()
                        .any(|t| matches!(t.text, "target_arch" | "target_feature"))
                {
                    add(&mut out, token.line, "arch_cfg", "architecture predicate");
                }
            }
            "sort_by" | "sort_unstable_by" => {
                let open = i + 1;
                if tokens.get(open).is_some_and(|t| t.text == "(")
                    && let Some(end) = ends[open]
                    && tokens[open + 1..end].iter().any(|t| t.text == "partial_cmp")
                {
                    add(&mut out, token.line, "float_sort", "partial_cmp sort comparator");
                }
            }
            "addr"
                if i > 0 && tokens[i - 1].text == "." && tokens.get(i + 1).is_some_and(|t| t.text == "(") =>
            {
                add(&mut out, token.line, "address_order", "pointer addr");
            }
            "as" if tokens.get(i + 1).is_some_and(|t| t.text == "usize") => {
                // Only flag a syntactically visible pointer expression. Integer casts stay clean;
                // aliases and an earlier assignment of a raw pointer need typed review.
                let start = tokens[..i]
                    .iter()
                    .rposition(|t| matches!(t.text, ";" | "=" | "{" | "}"))
                    .map_or(0, |at| at + 1);
                let expr = &tokens[start..i];
                let pointer_cast = expr
                    .windows(3)
                    .any(|w| w[0].text == "as" && w[1].text == "*" && matches!(w[2].text, "const" | "mut"));
                if pointer_cast || expr.iter().any(|t| t.text == "as_ptr" || t.text == "as_mut_ptr") {
                    add(&mut out, token.line, "address_order", "pointer cast to usize");
                }
            }
            _ => {}
        }
    }
    out
}
