// SPDX-License-Identifier: MPL-2.0
//! UI text scan (ADR-8586): the string literals the editor draws as text carry no developer strings.
//!
//! The scan walks the same non-test sources as the agent parity scan
//! (`agent_parity::non_test_sources`), finds every string literal passed to a UI text call
//! (`ui.label`, `weak`, `colored_label`, `on_hover_text`, `hint_text`, `RichText::new`, ..., also
//! through `format!`) and refuses in it:
//!
//! - agent method syntax (`editor.procgen_open`, `render.sequence`, `camera.set_path`): people use
//!   the panel, the method belongs on the book/agents page (or after `Agent: ` in a tooltip, the
//!   convention of `tooltips.rs`);
//! - spec references (`(specs/27)`) and test fixture ids (`fixture:source`);
//! - internal tool names;
//! - raw snake_case error codes in parentheses (`(tables_panel_nothing_to_undo)`), and a
//!   `({})`/`({code})` placeholder filled from an error code: show the sentence, keep the code in
//!   the agent error and at most in a hover tooltip.
//!
//! `crates/regia_editor/ui-text-allow.txt` lists the legitimate exceptions, one per line:
//! `file | flagged text | reason`. An entry that no longer matches anything fails the scan too.

use std::path::PathBuf;

use regia_editor::agent_parity::{mask, non_test_sources};

/// The allowlist file, relative to the crate.
const ALLOW_FILE: &str = "ui-text-allow.txt";

/// Internal tool, team and product names that never belong in a UI string.
const INTERNAL_NAMES: &[&str] = &["Tincal"];

/// Calls whose string argument is drawn as text.
const UI_CALLS: &[&str] = &[
    "label",
    "weak",
    "small",
    "heading",
    "monospace",
    "strong",
    "colored_label",
    "on_hover_text",
    "on_disabled_hover_text",
    "on_hover_text_at_pointer",
    "hint_text",
    "button",
    "small_button",
    "selectable_label",
    "selectable_value",
    "checkbox",
    "radio",
    "radio_value",
    "collapsing",
    "menu_button",
    "selected_text",
    "hyperlink_to",
];

/// `Type::new` constructors whose string argument is drawn as text.
const UI_TYPES: &[&str] = &[
    "RichText",
    "Label",
    "Button",
    "CollapsingHeader",
    "Checkbox",
    "SelectableLabel",
    "Hyperlink",
];

/// Formatting macros the scan looks through to the UI call around them.
const FORMAT_MACROS: &[&str] = &["format!", "write!", "writeln!"];

/// One string literal drawn as UI text.
#[derive(Debug, Clone)]
struct UiLiteral {
    file: String,
    line: usize,
    /// The literal's text, string continuations (`\` + newline + indent) joined.
    content: String,
    /// The innermost call around the literal (`label`, `format!`, ...).
    call: String,
    /// The code of that call's other arguments (strings blanked).
    args: String,
}

/// One refused piece of UI text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Finding {
    file: String,
    line: usize,
    /// The flagged text (the allowlist key with the file).
    flagged: String,
    /// The rule it broke.
    rule: &'static str,
}

fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The callee path in front of the `(` at `open` (`ui.label`, `egui::RichText::new`, `format!`).
fn callee(masked: &str, open: usize) -> &str {
    let head = masked[..open].trim_end();
    let bytes = head.as_bytes();
    let mut start = bytes.len();
    while start > 0 {
        let b = bytes[start - 1];
        if ident_byte(b) || b == b':' || b == b'.' || b == b'!' {
            start -= 1;
        } else {
            break;
        }
    }
    &head[start..]
}

/// The `(` of the call that encloses byte `at`, if the innermost bracket around it is a `(`.
fn enclosing_paren(masked: &str, at: usize) -> Option<usize> {
    let bytes = masked.as_bytes();
    let mut depth = 0usize;
    let mut i = at;
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b')' | b']' | b'}' => depth += 1,
            b'(' | b'[' | b'{' => {
                if depth == 0 {
                    return (bytes[i] == b'(').then_some(i);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

/// The matching `)` of the `(` at `open`.
fn closing_paren(masked: &str, open: usize) -> usize {
    let bytes = masked.as_bytes();
    let mut depth = 0usize;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    bytes.len()
}

/// Whether the callee path draws its string argument as text.
fn is_ui_callee(path: &str) -> bool {
    let path = path.trim_end_matches('!');
    let mut parts = path.rsplit(['.', ':']).filter(|p| !p.is_empty());
    let last = parts.next().unwrap_or("");
    if last == "new" {
        return parts.next().is_some_and(|ty| UI_TYPES.contains(&ty));
    }
    UI_CALLS.contains(&last)
}

/// `content` with string continuations joined and `\"` unescaped.
fn normalize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('\n') => {
                    while chars.peek().is_some_and(|c| c.is_whitespace()) {
                        chars.next();
                    }
                }
                Some('"') => {
                    out.push('"');
                    chars.next();
                }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Every string literal of one file's (masked) code that a UI text call draws.
fn ui_literals(file: &str, text: &str, masked: &str) -> Vec<UiLiteral> {
    let bytes = masked.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let open = i;
        let Some(rel) = masked[open + 1..].find('"') else {
            break;
        };
        let close = open + 1 + rel;
        i = close + 1;
        // The call around the literal, looking through formatting macros.
        let Some(mut paren) = enclosing_paren(masked, open) else {
            continue;
        };
        let inner = callee(masked, paren).to_owned();
        let args_end = closing_paren(masked, paren);
        let args = masked[(close + 1).min(args_end)..args_end].to_owned();
        let mut path = inner.clone();
        let mut hops = 0;
        while FORMAT_MACROS.contains(&path.rsplit(['.', ':']).next().unwrap_or("")) && hops < 3 {
            let Some(outer) = enclosing_paren(masked, paren) else {
                break;
            };
            paren = outer;
            path = callee(masked, paren).to_owned();
            hops += 1;
        }
        if !is_ui_callee(&path) {
            continue;
        }
        out.push(UiLiteral {
            file: file.to_owned(),
            line: masked[..open].bytes().filter(|b| *b == b'\n').count() + 1,
            content: normalize(&text[open + 1..close]),
            call: inner,
            args,
        });
    }
    out
}

/// The `word` at `at..` made of identifier bytes.
fn word_at(text: &str, at: usize) -> &str {
    let end = text[at..]
        .bytes()
        .position(|b| !ident_byte(b))
        .map_or(text.len(), |n| at + n);
    &text[at..end]
}

/// A `(snake_case_code)` at `at` (the `(`): lowercase ascii, digits, at least one `_`.
fn paren_code_at(text: &str, at: usize) -> Option<&str> {
    let word = word_at(text, at + 1);
    let is_code = word.contains('_')
        && word.starts_with(|c: char| c.is_ascii_lowercase())
        && word
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !word.ends_with('_');
    (is_code && text[at + 1 + word.len()..].starts_with(')')).then_some(&text[at..at + word.len() + 2])
}

/// Whether the call's other arguments name an error code (`e.code`, `code`, `reason(&e)`).
fn args_name_a_code(args: &str) -> bool {
    args.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|token| token == "code" || token == "reason")
}

/// The rules one UI literal breaks.
fn check(lit: &UiLiteral) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut push = |flagged: String, rule: &'static str| {
        out.push(Finding {
            file: lit.file.clone(),
            line: lit.line,
            flagged,
            rule,
        });
    };
    let text = lit.content.as_str();
    let bytes = text.as_bytes();
    for prefix in ["editor.", "render.", "camera."] {
        let mut from = 0;
        while let Some(rel) = text[from..].find(prefix) {
            let at = from + rel;
            from = at + prefix.len();
            let before_ok =
                at == 0 || !(ident_byte(bytes[at - 1]) || matches!(bytes[at - 1], b'.' | b'/' | b'\\'));
            let next = bytes.get(at + prefix.len()).copied();
            // The tooltip convention `Agent: <method>` (`tooltips.rs`) names the method on purpose.
            let agent_line = text[..at].contains("Agent: ");
            if before_ok && !agent_line && next.is_some_and(|b| b.is_ascii_lowercase()) {
                let method = word_at(text, at + prefix.len());
                push(format!("{prefix}{method}"), "agent method name in UI text");
            }
        }
    }
    if text.contains("(specs/") {
        push("(specs/".to_owned(), "spec reference in UI text");
    }
    if let Some(at) = text.find("fixture:") {
        push(
            format!("fixture:{}", word_at(text, at + "fixture:".len())),
            "test fixture id in UI text",
        );
    }
    for name in INTERNAL_NAMES {
        if text.contains(name) {
            push((*name).to_owned(), "internal tool name in UI text");
        }
    }
    for (at, _) in text.match_indices('(') {
        if let Some(code) = paren_code_at(text, at) {
            push(code.to_owned(), "raw error code in UI text");
        }
    }
    for placeholder in ["({code})", "({reason})"] {
        if text.contains(placeholder) {
            push(placeholder.to_owned(), "error code placeholder in UI text");
        }
    }
    if text.contains("({})") && FORMAT_MACROS.contains(&lit.call.as_str()) && args_name_a_code(&lit.args) {
        push("({})".to_owned(), "error code placeholder in UI text");
    }
    out
}

/// One allowlist row.
#[derive(Debug)]
struct Allow {
    file: String,
    flagged: String,
    reason: String,
}

fn parse_allow(text: &str) -> Result<Vec<Allow>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, " | ").map(str::trim).collect();
        let [file, flagged, reason] = parts[..] else {
            return Err(format!(
                "{ALLOW_FILE}:{}: want `file | flagged text | reason`",
                n + 1
            ));
        };
        if reason.is_empty() {
            return Err(format!("{ALLOW_FILE}:{}: a row needs a reason", n + 1));
        }
        out.push(Allow {
            file: file.to_owned(),
            flagged: flagged.to_owned(),
            reason: reason.to_owned(),
        });
    }
    Ok(out)
}

/// The findings `allow` does not cover, and the allowlist rows that cover nothing.
fn problems(findings: &[Finding], allow: &[Allow]) -> Vec<String> {
    let mut out = Vec::new();
    for f in findings {
        if !allow.iter().any(|a| a.file == f.file && a.flagged == f.flagged) {
            out.push(format!(
                "crates/regia_editor/src/{}:{}: {} `{}`: reword it for people (keep codes and method names in the agent error, the book page or an `Agent: ...` tooltip), or list it in {ALLOW_FILE} with a reason",
                f.file, f.line, f.rule, f.flagged
            ));
        }
    }
    for a in allow {
        if !findings
            .iter()
            .any(|f| f.file == a.file && f.flagged == a.flagged)
        {
            out.push(format!(
                "stale {ALLOW_FILE} row `{} | {}` ({}) matches nothing: drop it",
                a.file, a.flagged, a.reason
            ));
        }
    }
    out
}

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scan_editor() -> (usize, Vec<Finding>) {
    let sources = non_test_sources(&crate_dir().join("src")).expect("read the editor sources");
    let mut literals = 0;
    let mut findings = Vec::new();
    for source in &sources {
        for lit in ui_literals(&source.file, &source.text, &source.masked) {
            literals += 1;
            findings.extend(check(&lit));
        }
    }
    (literals, findings)
}

/// The editor's UI text carries no agent method names, spec references, fixture ids, internal
/// names or raw error codes outside the allowlist.
#[test]
fn editor_ui_text_has_no_developer_strings() {
    let (literals, findings) = scan_editor();
    assert!(
        literals > 1000,
        "the scan finds the editor's UI text: {literals} literals"
    );
    let path = crate_dir().join(ALLOW_FILE);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    let allow = parse_allow(&text).unwrap_or_else(|err| panic!("{err}"));
    let problems = problems(&findings, &allow);
    assert!(
        problems.is_empty(),
        "{} problem(s):\n{}",
        problems.len(),
        problems.join("\n")
    );
}

fn fixture_findings(src: &str) -> Vec<Finding> {
    let masked = mask(src);
    ui_literals("fixture.rs", src, &masked)
        .iter()
        .flat_map(check)
        .collect()
}

/// The acceptance fixture: each rule fires on a UI literal and stays quiet on non-UI code.
#[test]
fn the_scan_refuses_each_developer_string() {
    let src = r#"
fn draw(ui: &mut egui::Ui, e: &Error) {
    ui.weak("Open a graph (content browser, or editor.procgen_open) to edit it here.");
    ui.label("Load one with render.sequence or \
              camera.set_path.");
    ui.label("Reference path trace (specs/27).");
    ui.colored_label(c, "Type a rig (demo, fixture:source) on each side.");
    ui.weak("View a kit exported by Tincal.");
    ui.label("Nothing to undo (tables_panel_nothing_to_undo)");
    ui.colored_label(red, format!("{} ({})", e.message, e.code));
    ui.label(egui::RichText::new(format!("{message} ({code})")).strong());
}
fn quiet(ui: &mut egui::Ui, kind: &str, n: usize) {
    register("editor.procgen_open", "(specs/27)");
    ui.label(format!("{kind} ({})", n));
    ui.label("Save the level (Ctrl+S). The editor.");
    ui.label("assets/editor.ron");
    ui.heading("Rows (3)");
    ui.button("Copy").on_hover_text("Copy the code\nAgent: editor.matgraph_compile");
}
"#;
    let found = fixture_findings(src);
    let flagged: Vec<&str> = found.iter().map(|f| f.flagged.as_str()).collect();
    assert_eq!(
        flagged,
        [
            "editor.procgen_open",
            "render.sequence",
            "camera.set_path",
            "(specs/",
            "fixture:source",
            "Tincal",
            "(tables_panel_nothing_to_undo)",
            "({})",
            "({code})",
        ],
        "{found:#?}"
    );
    assert_eq!(found[1].line, 4, "{found:#?}");
}

/// An allowlist row covers its finding; a row that covers nothing is stale.
#[test]
fn allowlist_rows_cover_findings_and_go_stale() {
    let found = fixture_findings("fn f(ui: &mut egui::Ui) { ui.label(\"see (specs/27)\"); }");
    let allow = parse_allow("fixture.rs | (specs/ | a reason\n").expect("parses");
    assert!(problems(&found, &allow).is_empty());
    let stale = problems(&[], &allow);
    assert_eq!(stale.len(), 1);
    assert!(stale[0].starts_with("stale"), "{stale:?}");
    assert!(
        parse_allow("fixture.rs | (specs/ | \n").is_err(),
        "a row needs a reason"
    );
}
