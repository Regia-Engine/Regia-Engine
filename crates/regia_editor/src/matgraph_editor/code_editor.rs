// SPDX-License-Identifier: MPL-2.0
//! The Code node's code view (P-175, ADR-7852, `book/agents/material-graph-editor.md`): the
//! selected Code node's WGSL in a monospace editor with line numbers, keyword highlighting,
//! bracket matching, Find (Ctrl+F), inline error markers from naga at their line and column
//! with the messages in a list, and a completion list of the node's inputs, the project's
//! include helpers and the common WGSL built-ins.
//!
//! The text is a buffer: Ctrl+Enter (or **Apply**), or a pause of [`APPLY_AFTER_FRAMES`]
//! frames after the last keystroke, applies it to the node as one undoable edit (consecutive
//! applies to the same node coalesce), and the preview recompiles. The markers update while
//! typing: the buffer is checked as the compile would check it, includes read from the
//! project's `shaders/include/`. A node that fails shades magenta in the preview and refuses to
//! cook (`regia_matgraph::code`).
//!
//! Agent twins: `editor.matgraph_code_set { node, code?, inputs?, textures?, output? }` (the same
//! edit as Apply and the declaration rows) and `editor.matgraph_code_errors { node? }` (the
//! list the code view shows).

use std::collections::{BTreeMap, BTreeSet};

use egui::text::{LayoutJob, TextFormat};
use serde_json::{Value, json};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_matgraph::code::{self, CODE_SAMPLER, CodeDiagnostic, WGSL_BUILTINS, WGSL_KEYWORDS, WGSL_TYPES};
use regia_matgraph::{CodeInput, MaterialGraph, Node, ValueType};

use super::agent::{code_error, edit_answer, open_editor, refused, required};
use super::{MatGraphEditor, PendingEdit, edit};

/// Frames without a keystroke after which the buffer applies itself.
pub const APPLY_AFTER_FRAMES: u64 = 30;

/// Most identifiers the completion list shows.
pub const MAX_COMPLETIONS: usize = 8;

/// What a span of code is, for highlighting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// A WGSL keyword.
    Keyword,
    /// A WGSL type name.
    Type,
    /// A WGSL built-in function.
    Builtin,
    /// One of the node's inputs (or its sampler).
    Input,
    /// A number literal.
    Number,
    /// A comment.
    Comment,
    /// An `#include` (or resolved) directive line.
    Directive,
    /// A bracket.
    Bracket,
    /// Anything else.
    Plain,
}

/// Splits `text` into `(byte range, token)` spans covering it.
#[must_use]
pub fn tokens(text: &str, inputs: &BTreeSet<String>) -> Vec<(std::ops::Range<usize>, Token)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line_start = true;
    while i < bytes.len() {
        let start = i;
        let c = bytes[i];
        let token = if line_start && text[i..].trim_start_matches([' ', '\t']).starts_with('#') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            Token::Directive
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            Token::Comment
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            let mut nest = 1;
            while i < bytes.len() && nest > 0 {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    nest += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    nest -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Token::Comment
        } else if c.is_ascii_digit() || (c == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            while i < bytes.len() {
                let b = bytes[i];
                let exponent_sign =
                    (b == b'+' || b == b'-') && i > start && matches!(bytes[i - 1], b'e' | b'E');
                if b.is_ascii_alphanumeric() || b == b'.' || exponent_sign {
                    i += 1;
                } else {
                    break;
                }
            }
            Token::Number
        } else if c.is_ascii_alphabetic() || c == b'_' {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &text[start..i];
            if WGSL_KEYWORDS.contains(&word) {
                Token::Keyword
            } else if WGSL_TYPES.contains(&word) {
                Token::Type
            } else if inputs.contains(word) || word == CODE_SAMPLER {
                Token::Input
            } else if WGSL_BUILTINS.contains(&word) {
                Token::Builtin
            } else {
                Token::Plain
            }
        } else if matches!(c, b'(' | b')' | b'{' | b'}' | b'[' | b']') {
            i += 1;
            Token::Bracket
        } else {
            // One character (all of a multi-byte one).
            i += text[i..].chars().next().map_or(1, char::len_utf8);
            Token::Plain
        };
        // Directives and comments stop before the newline, so only a newline starts a line.
        line_start = token == Token::Plain && c == b'\n';
        out.push((start..i, token));
    }
    out
}

/// The bracket matching the one at or just before character `cursor`, as two character
/// indices `(at, match)`. Comments are not skipped (good enough for a highlight).
#[must_use]
pub fn matching_bracket(text: &str, cursor: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let pair = |c: char| match c {
        '(' => Some((')', true)),
        '{' => Some(('}', true)),
        '[' => Some((']', true)),
        ')' => Some(('(', false)),
        '}' => Some(('{', false)),
        ']' => Some(('[', false)),
        _ => None,
    };
    let at = [cursor, cursor.wrapping_sub(1)]
        .into_iter()
        .find(|&i| chars.get(i).copied().and_then(pair).is_some())?;
    let open = chars[at];
    let (close, forward) = pair(open)?;
    let mut depth = 0i32;
    if forward {
        for (i, &c) in chars.iter().enumerate().skip(at) {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some((at, i));
                }
            }
        }
    } else {
        for (i, &c) in chars[..=at].iter().enumerate().rev() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some((at, i));
                }
            }
        }
    }
    None
}

/// Every case-insensitive occurrence of `needle` in `text`, as character ranges.
#[must_use]
pub fn find_matches(text: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let hay: Vec<char> = text.chars().map(|c| c.to_ascii_lowercase()).collect();
    let pin: Vec<char> = needle.chars().map(|c| c.to_ascii_lowercase()).collect();
    hay.windows(pin.len())
        .enumerate()
        .filter(|(_, w)| *w == pin.as_slice())
        .map(|(i, _)| (i, i + pin.len()))
        .collect()
}

/// The identifier ending at character `cursor`: its first character index and its text.
#[must_use]
pub fn word_before(text: &str, cursor: usize) -> (usize, String) {
    let chars: Vec<char> = text.chars().take(cursor).collect();
    let start = chars
        .iter()
        .rposition(|c| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map_or(0, |p| p + 1);
    (start, chars[start..].iter().collect())
}

/// The completion list for `prefix` from `candidates` (inputs first, as given): identifiers
/// that start with it (case-sensitive), longer than it, at most [`MAX_COMPLETIONS`].
#[must_use]
pub fn completions(prefix: &str, candidates: &[String]) -> Vec<String> {
    if prefix.is_empty() || prefix.starts_with(|c: char| c.is_ascii_digit()) {
        return Vec::new();
    }
    let mut seen = BTreeSet::new();
    candidates
        .iter()
        .filter(|c| c.starts_with(prefix) && c.len() > prefix.len())
        .filter(|c| seen.insert(c.as_str()))
        .take(MAX_COMPLETIONS)
        .cloned()
        .collect()
}

/// The identifiers a node's code may use: its inputs, its texture inputs and sampler, the
/// include helpers, then the WGSL built-ins.
#[must_use]
pub fn candidates(node: &Node, helpers: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if let Node::Code { inputs, textures, .. } = node {
        out.extend(inputs.iter().map(|i| i.name.clone()));
        out.extend(textures.iter().cloned());
        if !textures.is_empty() {
            out.push(CODE_SAMPLER.to_string());
        }
    }
    out.extend(helpers.iter().cloned());
    out.extend(WGSL_BUILTINS.iter().map(|s| (*s).to_string()));
    out
}

/// The character index where 1-based `line` starts (the end for a line past it).
#[must_use]
pub fn line_start(text: &str, line: u32) -> usize {
    if line <= 1 {
        return 0;
    }
    let mut seen = 1;
    for (i, c) in text.chars().enumerate() {
        if c == '\n' {
            seen += 1;
            if seen == line {
                return i + 1;
            }
        }
    }
    text.chars().count()
}

/// The function names every `*.wgsl` in the project's include directory declares, sorted.
#[must_use]
pub fn project_helpers(root: Option<&std::path::Path>) -> Vec<String> {
    let Some(root) = root else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(root.join(code::CODE_INCLUDE_DIR)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "wgsl"))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .flat_map(|text| code::declared_functions(&text))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The include texts `code` names, read from the project (`None` for one that cannot be read).
fn read_includes(code: &str, root: Option<&std::path::Path>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(root) = root else {
        return out;
    };
    let mut read = code::project_include_reader(root);
    for name in code::include_names(code) {
        if let Ok(text) = read(&name) {
            out.insert(name, text);
        }
    }
    out
}

/// The diagnostics the compile reports for Code node `node` of `graph` with `text` as its code
/// (includes read from the project).
#[must_use]
pub fn diagnostics_for(
    graph: &MaterialGraph,
    node: &Node,
    text: &str,
    root: Option<&std::path::Path>,
) -> Vec<CodeDiagnostic> {
    let Node::Code {
        inputs,
        textures,
        output,
        ..
    } = node
    else {
        return Vec::new();
    };
    let resolved = code::with_resolved(text, &read_includes(text, root));
    code::check_node(
        graph,
        &Node::Code {
            inputs: inputs.clone(),
            textures: textures.clone(),
            output: *output,
            code: resolved,
        },
    )
}

/// The code view's buffer and view state, kept in egui memory per panel.
#[derive(Clone, Debug, Default)]
pub struct CodeEditorState {
    /// The node the buffer belongs to.
    pub node: String,
    /// The text being edited.
    pub text: String,
    /// The node's code the buffer was loaded from or last applied.
    pub base: String,
    /// Frame of the last keystroke while the buffer differs from `base`.
    pub edited_frame: Option<u64>,
    /// Cursor (character index) after the last frame.
    pub cursor: Option<usize>,
    /// Find bar open.
    pub find_open: bool,
    /// Find text.
    pub find: String,
    /// The match the cursor went to last.
    pub find_index: usize,
    /// Move the cursor here (character index) on the next frame.
    pub goto: Option<usize>,
    /// Focus the find field on the next frame.
    pub focus_find: bool,
    /// The find field had focus at the end of the last frame (egui may drop focus on Esc
    /// before the panel sees the key).
    pub find_had_focus: bool,
    /// Include helper names, read when the node was selected (or on Apply).
    pub helpers: Option<Vec<String>>,
    /// `(text, diagnostics)` of the last check.
    pub checked: Option<(String, Vec<CodeDiagnostic>)>,
    /// Frame the state was last drawn (egui may run a panel twice in one frame).
    pub frame: u64,
}

/// The egui id of the code view's text field (tests focus it).
#[must_use]
pub fn text_id(node: &str) -> egui::Id {
    egui::Id::new(("matgraph-code-text", node))
}

fn state_id() -> egui::Id {
    egui::Id::new("matgraph-code-state")
}

/// Colors of each token, from the theme.
fn token_color(tokens: &crate::theme::ThemeTokens, token: Token) -> egui::Color32 {
    match token {
        Token::Keyword => tokens.accents.active,
        Token::Type => tokens.accents.log_info,
        Token::Builtin => tokens.widgets.code,
        Token::Input => tokens.signal,
        Token::Number => tokens.accents.ok,
        Token::Comment => tokens.muted,
        Token::Directive => tokens.accents.log_warn,
        Token::Bracket => tokens.strong,
        Token::Plain => tokens.text,
    }
}

/// The highlighted layout of `text`: tokens colored, the matched bracket pair and the Find
/// matches on a background, error lines underlined.
fn layout(
    ui: &egui::Ui,
    text: &str,
    inputs: &BTreeSet<String>,
    marks: &[(usize, usize)],
    error_lines: &BTreeSet<u32>,
) -> LayoutJob {
    let theme = crate::theme::ui_tokens(ui.ctx());
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    // Character index → byte offset for the marks.
    let byte_of: Vec<usize> = text
        .char_indices()
        .map(|(b, _)| b)
        .chain(std::iter::once(text.len()))
        .collect();
    let marked: Vec<(usize, usize)> = marks
        .iter()
        .filter_map(|&(a, b)| Some((*byte_of.get(a)?, *byte_of.get(b)?)))
        .collect();
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let mut line = 1u32;
    for (range, token) in tokens(text, inputs) {
        // Split each token at mark edges and line ends so backgrounds and underlines are exact.
        let mut cuts: Vec<usize> = vec![range.start, range.end];
        for &(a, b) in &marked {
            for x in [a, b] {
                if x > range.start && x < range.end {
                    cuts.push(x);
                }
            }
        }
        for (i, _) in text[range.clone()].match_indices('\n') {
            cuts.push(range.start + i + 1);
        }
        cuts.sort_unstable();
        cuts.dedup();
        for pair in cuts.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if a >= b || !text.is_char_boundary(a) || !text.is_char_boundary(b) {
                continue;
            }
            let piece = &text[a..b];
            let in_mark = marked.iter().any(|&(x, y)| a >= x && b <= y);
            let mut format = TextFormat {
                font_id: font.clone(),
                color: token_color(&theme, token),
                ..TextFormat::default()
            };
            if in_mark {
                format.background = theme.accents.select.gamma_multiply(0.45);
            }
            if error_lines.contains(&line) && !piece.trim().is_empty() {
                format.underline = egui::Stroke::new(1.0, theme.accents.error_text);
            }
            job.append(piece, 0.0, format);
            line += u32::try_from(piece.matches('\n').count()).unwrap_or(0);
        }
    }
    job
}

/// The line-number gutter: one number per line, error lines in the error color with a `!`.
fn gutter(ui: &egui::Ui, lines: u32, error_lines: &BTreeSet<u32>) -> LayoutJob {
    let theme = crate::theme::ui_tokens(ui.ctx());
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let width = lines.max(1).to_string().len();
    let mut job = LayoutJob::default();
    for line in 1..=lines.max(1) {
        let error = error_lines.contains(&line);
        let text = format!(
            "{}{line:>width$}{}",
            if error { "!" } else { " " },
            if line < lines.max(1) { "\n" } else { "" }
        );
        job.append(
            &text,
            0.0,
            TextFormat {
                font_id: font.clone(),
                color: if error {
                    theme.accents.error_text
                } else {
                    theme.muted
                },
                ..TextFormat::default()
            },
        );
    }
    job
}

/// One `name: type` declaration as the signature line shows it.
fn signature(inputs: &[CodeInput], textures: &[String], output: ValueType) -> String {
    let mut parts: Vec<String> = inputs
        .iter()
        .map(|i| format!("{}: {}", i.name, i.ty.wgsl()))
        .collect();
    parts.extend(textures.iter().map(|t| format!("{t}: texture_2d<f32>")));
    if !textures.is_empty() {
        parts.push(format!("{CODE_SAMPLER}: sampler"));
    }
    format!("fn({}) -> {}", parts.join(", "), output.wgsl())
}

const TYPES: [ValueType; 4] = [
    ValueType::Float,
    ValueType::Vec2,
    ValueType::Vec3,
    ValueType::Vec4,
];

fn type_combo(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    label: &str,
    ty: &mut ValueType,
) -> bool {
    let mut changed = false;
    let response = egui::ComboBox::from_id_salt(id)
        .selected_text(format!("{ty:?}"))
        .show_ui(ui, |ui| {
            for t in TYPES {
                changed |= ui.selectable_value(ty, t, format!("{t:?}")).changed();
            }
        })
        .response;
    crate::a11y_audit::name(response, egui::WidgetType::ComboBox, label);
    changed
}

/// The declaration rows of a Code node (inputs with name and type, texture inputs, output
/// type). Each change is one edit through the same path as `editor.matgraph_code_set`.
fn declaration_ui(
    ui: &mut egui::Ui,
    graph: &MaterialGraph,
    name: &str,
    node: &Node,
    pending: &mut Vec<PendingEdit>,
) {
    let Node::Code {
        inputs,
        textures,
        output,
        ..
    } = node
    else {
        return;
    };
    let mut new_inputs = inputs.clone();
    let mut new_textures = textures.clone();
    let mut new_output = *output;
    let mut changed = false;
    let mut remove_input = None;
    for (i, input) in new_inputs.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            let field = ui.add(egui::TextEdit::singleline(&mut input.name).desired_width(90.0));
            crate::a11y_audit::name(field.clone(), egui::WidgetType::TextEdit, "Input name");
            // Each keystroke is an edit; they coalesce into one undo step (merge key).
            changed |= field.changed();
            changed |= type_combo(
                ui,
                ("matgraph-code-input-ty", name, i),
                "Input type",
                &mut input.ty,
            );
            if ui.button("Remove").on_hover_text("Remove this input").clicked() {
                remove_input = Some(i);
            }
        });
    }
    if let Some(i) = remove_input {
        new_inputs.remove(i);
        changed = true;
    }
    ui.horizontal(|ui| {
        if ui.button("Add input").clicked() {
            let mut n = new_inputs.len();
            let fresh = loop {
                let candidate = format!("in{n}");
                if !new_inputs.iter().any(|i| i.name == candidate) {
                    break candidate;
                }
                n += 1;
            };
            new_inputs.push(CodeInput {
                name: fresh,
                ty: ValueType::Float,
                node: String::new(),
            });
            changed = true;
        }
        ui.label("Returns");
        changed |= type_combo(ui, ("matgraph-code-output", name), "Output type", &mut new_output);
    });
    let declared: Vec<String> = graph.textures.iter().map(|t| t.name.clone()).collect();
    let mut remove_texture = None;
    for (i, texture) in new_textures.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.monospace(format!("{texture}: texture_2d<f32>"));
            if ui
                .button("Remove")
                .on_hover_text("Remove this texture input")
                .clicked()
            {
                remove_texture = Some(i);
            }
        });
    }
    if let Some(i) = remove_texture {
        new_textures.remove(i);
        changed = true;
    }
    let free: Vec<&String> = declared.iter().filter(|d| !new_textures.contains(d)).collect();
    if !free.is_empty() {
        let mut pick: Option<String> = None;
        let response = egui::ComboBox::from_id_salt(("matgraph-code-add-texture", name))
            .selected_text("Add texture input")
            .show_ui(ui, |ui| {
                for d in free {
                    if ui.selectable_label(false, d.as_str()).clicked() {
                        pick = Some(d.clone());
                    }
                }
            })
            .response;
        crate::a11y_audit::name(response, egui::WidgetType::ComboBox, "Add texture input");
        if let Some(t) = pick {
            new_textures.push(t);
            changed = true;
        }
    }
    if changed {
        let n = name.to_string();
        pending.push(PendingEdit::new(
            "edit code node",
            Some(format!("code-decl:{name}")),
            move |d| {
                set_code(
                    d,
                    &n,
                    None,
                    Some(new_inputs),
                    Some(new_textures),
                    Some(new_output),
                )
            },
        ));
    }
}

/// Sets a Code node's code and declaration (what is `Some`); inputs keep their wire by name.
///
/// # Errors
///
/// No node `name`, or it is not a Code node.
pub fn set_code(
    doc: &mut super::MatGraphDoc,
    name: &str,
    text: Option<String>,
    inputs: Option<Vec<CodeInput>>,
    textures: Option<Vec<String>>,
    output: Option<ValueType>,
) -> Result<(), String> {
    let Some(node) = doc.graph.nodes.get(name).cloned() else {
        return Err(format!("no node `{name}`"));
    };
    let Node::Code {
        inputs: old_inputs,
        textures: old_textures,
        output: old_output,
        code: old_code,
    } = node
    else {
        return Err(format!("`{name}` is a {} node, not a Code node", node.kind()));
    };
    let inputs = inputs.map_or_else(
        || old_inputs.clone(),
        |list| {
            list.into_iter()
                .enumerate()
                .map(|(i, mut input)| {
                    if input.node.is_empty() {
                        // Keep the wire of the input of that name (or, after a rename, at that
                        // position).
                        input.node = old_inputs
                            .iter()
                            .find(|o| o.name == input.name)
                            .or_else(|| old_inputs.get(i))
                            .map(|o| o.node.clone())
                            .unwrap_or_default();
                    }
                    input
                })
                .collect()
        },
    );
    doc.set_node(
        name,
        Node::Code {
            inputs,
            textures: textures.unwrap_or(old_textures),
            output: output.unwrap_or(old_output),
            code: text.map_or(old_code, |t| code::strip_resolved(&t)),
        },
    )
}

/// Draws the selected Code node's code view (the inspector calls it below the node's rows).
pub fn code_editor_ui(
    ui: &mut egui::Ui,
    editor: &mut MatGraphEditor,
    name: &str,
    node: &Node,
    pending: &mut Vec<PendingEdit>,
) {
    let Node::Code {
        inputs,
        textures,
        output,
        code: doc_code,
    } = node
    else {
        return;
    };
    let ctx = ui.ctx().clone();
    let frame = ctx.cumulative_frame_nr();
    let mut st: CodeEditorState =
        ctx.data_mut(|d| d.get_temp_mut_or_default::<CodeEditorState>(state_id()).clone());
    let first_pass = st.frame != frame;
    st.frame = frame;
    if st.node != name || (*doc_code != st.base) {
        // Another node, or the document changed under the buffer (undo, the agent).
        let keep_find = (st.find_open, st.find.clone());
        st = CodeEditorState {
            node: name.to_string(),
            text: doc_code.clone(),
            base: doc_code.clone(),
            find_open: keep_find.0,
            find: keep_find.1,
            frame,
            ..CodeEditorState::default()
        };
    }
    if st.helpers.is_none() {
        st.helpers = Some(project_helpers(editor.root.as_deref()));
    }
    let id = text_id(name);
    let text_focused = ctx.memory(|m| m.has_focus(id));
    let find_id = egui::Id::new(("matgraph-code-find", name));
    let find_focused = ctx.memory(|m| m.has_focus(find_id));
    let mut apply_now = false;
    let mut complete_first = false;
    let find_focus_last = st.find_had_focus;
    if text_focused || find_focused || find_focus_last {
        ui.input_mut(|i| {
            if i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter) {
                apply_now = true;
            }
            if i.consume_key(egui::Modifiers::COMMAND, egui::Key::F) {
                st.find_open = true;
                st.focus_find = true;
            }
            if text_focused && i.consume_key(egui::Modifiers::COMMAND, egui::Key::Space) {
                complete_first = true;
            }
            if (find_focused || find_focus_last) && i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                st.find_open = false;
                st.goto = st.cursor;
            }
        });
    }

    ui.separator();
    ui.strong("Code");
    ui.monospace(signature(inputs, textures, *output));
    declaration_ui(ui, &editor.doc.graph, name, node, pending);
    ui.horizontal(|ui| {
        if ui
            .button("Apply")
            .on_hover_text("Apply the code to the node now (Ctrl+Enter); it also applies after a short pause")
            .clicked()
        {
            apply_now = true;
        }
        if ui
            .button("Find")
            .on_hover_text("Find in the code (Ctrl+F)")
            .clicked()
        {
            st.find_open = !st.find_open;
            st.focus_find = st.find_open;
        }
        if st.text != st.base {
            ui.weak("Unapplied changes");
        }
    });
    let matches = if st.find_open {
        find_matches(&st.text, &st.find)
    } else {
        Vec::new()
    };
    if st.find_open {
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut st.find)
                    .id(find_id)
                    .hint_text("Find")
                    .desired_width(140.0),
            );
            crate::a11y_audit::name(field.clone(), egui::WidgetType::TextEdit, "Find in code");
            if st.focus_find {
                field.request_focus();
                st.focus_find = false;
            }
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.weak(if matches.is_empty() {
                "No matches".to_string()
            } else {
                format!(
                    "{} of {}",
                    st.find_index.min(matches.len() - 1) + 1,
                    matches.len()
                )
            });
            if (ui
                .button("Next")
                .on_hover_text("Go to the next match (Enter)")
                .clicked()
                || enter)
                && !matches.is_empty()
            {
                let from = st.cursor.unwrap_or(0);
                let next = matches.iter().position(|&(a, _)| a > from).unwrap_or(0);
                st.find_index = next;
                st.goto = Some(matches[next].0);
                if enter {
                    field.request_focus();
                }
            }
            if ui.button("Close").on_hover_text("Close Find (Esc)").clicked() {
                st.find_open = false;
            }
            st.find_had_focus = field.has_focus();
        });
    } else {
        st.find_had_focus = false;
    }

    // Live check of the buffer (memoized by content).
    if st.checked.as_ref().is_none_or(|(t, _)| *t != st.text) {
        let diags = diagnostics_for(&editor.doc.graph, node, &st.text, editor.root.as_deref());
        st.checked = Some((st.text.clone(), diags));
    }
    let diags = st.checked.as_ref().map(|(_, d)| d.clone()).unwrap_or_default();
    let error_lines: BTreeSet<u32> = diags.iter().map(|d| d.line).filter(|&l| l > 0).collect();
    let mut marks: Vec<(usize, usize)> = matches.clone();
    if let Some((a, b)) = st.cursor.and_then(|c| matching_bracket(&st.text, c)) {
        marks.push((a, a + 1));
        marks.push((b, b + 1));
    }
    let input_names: BTreeSet<String> = inputs
        .iter()
        .map(|i| i.name.clone())
        .chain(textures.iter().cloned())
        .collect();
    let lines = u32::try_from(st.text.split('\n').count()).unwrap_or(u32::MAX);
    let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, _wrap: f32| {
        let job = layout(ui, text.as_str(), &input_names, &marks, &error_lines);
        ui.ctx().fonts_mut(|f| f.layout_job(job))
    };
    let before = st.text.clone();
    let output = egui::ScrollArea::both()
        .id_salt(("matgraph-code-scroll", name))
        .max_height(260.0)
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let gutter_job = gutter(ui, lines, &error_lines);
                ui.add(egui::Label::new(gutter_job).selectable(false));
                let response = egui::TextEdit::multiline(&mut st.text)
                    .id(id)
                    .code_editor()
                    .lock_focus(true)
                    .desired_rows(8)
                    .desired_width(f32::INFINITY)
                    .layouter(&mut layouter)
                    .show(ui);
                crate::a11y_audit::name(
                    response.response.response.clone(),
                    egui::WidgetType::TextEdit,
                    "Code",
                );
                response
            })
            .inner
        })
        .inner;
    let mut state = output.state.clone();
    if let Some(range) = output.cursor_range {
        st.cursor = Some(range.primary.index.0);
    }
    if complete_first {
        let cursor = st.cursor.unwrap_or_else(|| st.text.chars().count());
        let (start, prefix) = word_before(&st.text, cursor);
        let helpers = st.helpers.clone().unwrap_or_default();
        if let Some(first) = completions(&prefix, &candidates(node, &helpers)).first() {
            st.goto = Some(insert_completion(&mut st.text, start, cursor, first));
        }
    }
    if st.text != before && first_pass {
        st.edited_frame = Some(frame);
        ctx.request_repaint();
    }
    // The completion list for the word before the cursor.
    if text_focused || complete_first {
        let cursor = st.cursor.unwrap_or(0);
        let (start, prefix) = word_before(&st.text, cursor);
        let helpers = st.helpers.clone().unwrap_or_default();
        let list = completions(&prefix, &candidates(node, &helpers));
        if !list.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.weak("Complete (Ctrl+Space):");
                for word in &list {
                    if ui
                        .small_button(word)
                        .on_hover_text(format!("Insert `{word}`"))
                        .clicked()
                    {
                        st.goto = Some(insert_completion(&mut st.text, start, cursor, word));
                        st.edited_frame = Some(frame);
                    }
                }
            });
        }
    }
    if let Some(at) = st.goto.take() {
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(at))));
        state.store(&ctx, id);
        ctx.memory_mut(|m| m.request_focus(id));
        st.cursor = Some(at);
    }

    // Errors with their line and column; a click (or Enter) moves the cursor there.
    let theme = crate::theme::ui_tokens(ui.ctx());
    if diags.is_empty() {
        ui.colored_label(theme.accents.ok, "The code checks out.");
    } else {
        for d in &diags {
            let label = if d.line == 0 {
                format!("Declaration: {}", d.message)
            } else {
                format!("Line {}, column {}: {}", d.line, d.column, d.message)
            };
            let response = ui.add(egui::Button::selectable(
                false,
                egui::RichText::new(label).color(theme.accents.error_text),
            ));
            if response.clicked() && d.line > 0 {
                st.goto = Some(line_start(&st.text, d.line) + d.column.saturating_sub(1) as usize);
            }
        }
    }

    // Apply: Ctrl+Enter or Apply now; otherwise after a pause with no keystroke.
    let idle = st
        .edited_frame
        .is_some_and(|f| frame.saturating_sub(f) >= APPLY_AFTER_FRAMES);
    if st.text != st.base {
        if apply_now || idle {
            let (n, text) = (name.to_string(), st.text.clone());
            pending.push(PendingEdit::new(
                "edit code",
                Some(format!("code:{name}")),
                move |d| set_code(d, &n, Some(text), None, None, None),
            ));
            st.base = st.text.clone();
            st.edited_frame = None;
            if apply_now {
                st.helpers = None;
            }
        } else {
            ctx.request_repaint();
        }
    } else if apply_now {
        st.helpers = None;
    }
    ctx.data_mut(|d| d.insert_temp(state_id(), st));
}

/// Replaces the characters `start..cursor` of `text` with `word`; returns the character index
/// after it.
fn insert_completion(text: &mut String, start: usize, cursor: usize, word: &str) -> usize {
    let byte = |i: usize| text.char_indices().nth(i).map_or(text.len(), |(b, _)| b);
    let (a, b) = (byte(start), byte(cursor));
    text.replace_range(a..b, word);
    start + word.chars().count()
}

// ---- Agent surface ----

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

const fn p(name: &'static str, kind: &'static str, required: bool, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

/// The Code node's agent methods with their handlers.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.matgraph_code_set",
                tool: "matgraph_code_set",
                description: "Set a Code node's WGSL body and, optionally, its declaration (typed inputs, texture inputs, output type) in one undoable edit, as the code view's Apply does. Inputs keep their wire by name. Answers the node's code diagnostics (line, column, message) beside the graph's errors.",
                params: &const {
                    [
                        p("node", "string", true, "The Code node."),
                        p(
                            "code",
                            "string",
                            false,
                            "The function body (WGSL statements ending in a return); `#include \"name.wgsl\"` lines read shaders/include/.",
                        ),
                        p(
                            "inputs",
                            "array",
                            false,
                            "[{ name, ty: Float | Vec2 | Vec3 | Vec4, node? }] in parameter order.",
                        ),
                        p(
                            "textures",
                            "array",
                            false,
                            "Declared graph textures the code may sample, in parameter order.",
                        ),
                        p("output", "string", false, "Float | Vec2 | Vec3 | Vec4."),
                    ]
                },
                result: "{ node, code_ok, code_errors: [{ code, line, column, include, message }], revision, ok, errors }",
                mutating: true,
                permission: Permission::EditScene,
            },
            code_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.matgraph_code_errors",
                tool: "matgraph_code_errors",
                description: "The code diagnostics of the open graph's Code nodes (or one of them): naga's syntax and validation errors and the Code node restrictions, each at its line and column, as the code view lists them. A node with errors shades magenta in the preview and refuses to cook.",
                params: &const {
                    [p(
                        "node",
                        "string",
                        false,
                        "Only this Code node (default: every one).",
                    )]
                },
                result: "{ nodes: [{ node, ok, errors: [{ code, line, column, include, message }] }] }",
                mutating: false,
                permission: Permission::Read,
            },
            code_errors as Handler,
        ),
    ]
}

/// A diagnostic as the agent answers it.
#[must_use]
pub fn diagnostic_json(d: &CodeDiagnostic) -> Value {
    json!({
        "code": d.code,
        "line": d.line,
        "column": d.column,
        "include": d.include,
        "message": d.message,
    })
}

fn not_code(name: &str, kind: &str) -> RpcError {
    code_error(
        "matgraph_not_code_node",
        format!("`{name}` is a {kind} node, not a Code node"),
        "editor.matgraph_edit { op: \"add\", kind: \"Code\" } adds one",
    )
}

fn missing(name: &str) -> RpcError {
    code_error(
        "matgraph_node_missing",
        format!("no node `{name}`"),
        "editor.matgraph_state lists the nodes",
    )
}

fn parse_type(value: &str) -> Result<ValueType, RpcError> {
    Ok(match value {
        "Float" => ValueType::Float,
        "Vec2" => ValueType::Vec2,
        "Vec3" => ValueType::Vec3,
        "Vec4" => ValueType::Vec4,
        other => {
            return Err(code_error(
                "matgraph_code_bad_type",
                format!("`{other}` is not a value type"),
                "use Float, Vec2, Vec3 or Vec4",
            ));
        }
    })
}

fn node_diagnostics(editor: &MatGraphEditor, name: &str) -> Vec<CodeDiagnostic> {
    let Some(node) = editor.doc.graph.nodes.get(name) else {
        return Vec::new();
    };
    let Node::Code { code, .. } = node else {
        return Vec::new();
    };
    diagnostics_for(&editor.doc.graph, node, code, editor.root.as_deref())
}

/// `editor.matgraph_code_set`.
fn code_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!(target: "regia_editor", "matgraph_code_set").entered();
    let name = required(params, "node")?.to_string();
    {
        let editor = open_editor(world)?;
        match editor.doc.graph.nodes.get(&name) {
            None => return Err(missing(&name)),
            Some(Node::Code { .. }) => {}
            Some(other) => return Err(not_code(&name, other.kind())),
        }
    }
    let text = match params.get("code") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(RpcError::invalid_params("`code` must be a string")),
    };
    let inputs = match params.get("inputs") {
        None | Some(Value::Null) => None,
        Some(Value::Array(list)) => Some(
            list.iter()
                .map(|v| {
                    let name = v
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| RpcError::invalid_params("each input needs `name` (string)"))?;
                    let ty = parse_type(v.get("ty").and_then(Value::as_str).unwrap_or("Float"))?;
                    Ok(CodeInput {
                        name: name.to_string(),
                        ty,
                        node: v
                            .get("node")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    })
                })
                .collect::<Result<Vec<_>, RpcError>>()?,
        ),
        Some(_) => return Err(RpcError::invalid_params("`inputs` must be an array")),
    };
    let textures = match params.get("textures") {
        None | Some(Value::Null) => None,
        Some(_) => Some(super::agent::string_list(params, "textures")?),
    };
    let output = params
        .get("output")
        .and_then(Value::as_str)
        .map(parse_type)
        .transpose()?;
    let n = name.clone();
    edit(world, "edit code", Some(format!("code:{name}")), move |d| {
        set_code(d, &n, text, inputs, textures, output)
    })
    .map_err(refused)?;
    let diags = node_diagnostics(world.resource::<MatGraphEditor>(), &name);
    Ok(edit_answer(
        world,
        json!({
            "node": name,
            "code_ok": diags.is_empty(),
            "code_errors": diags.iter().map(diagnostic_json).collect::<Vec<_>>(),
        }),
    ))
}

/// `editor.matgraph_code_errors`.
fn code_errors(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!(target: "regia_editor", "matgraph_code_errors").entered();
    let editor = open_editor(world)?;
    let only = params.get("node").and_then(Value::as_str);
    if let Some(name) = only {
        match editor.doc.graph.nodes.get(name) {
            None => return Err(missing(name)),
            Some(Node::Code { .. }) => {}
            Some(other) => return Err(not_code(name, other.kind())),
        }
    }
    let nodes: Vec<Value> = editor
        .doc
        .graph
        .nodes
        .iter()
        .filter(|(n, node)| matches!(node, Node::Code { .. }) && only.is_none_or(|o| o == n.as_str()))
        .map(|(n, _)| {
            let diags = node_diagnostics(&editor, n);
            json!({
                "node": n,
                "ok": diags.is_empty(),
                "errors": diags.iter().map(diagnostic_json).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({ "nodes": nodes }))
}

/// Every error code the Code node's methods answer (beside the compile's own).
pub const AGENT_ERROR_CODES: [&str; 4] = [
    "matgraph_node_missing",
    "matgraph_not_code_node",
    "matgraph_code_bad_type",
    "matgraph_edit_refused",
];

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::{MatGraphDoc, apply_pending};

    const GRAPH: &str = r#"(
        format_version: 5,
        nodes: {
            "uv": TexCoord,
            "u": Swizzle(input: "uv", channels: "x"),
            "v": Swizzle(input: "uv", channels: "y"),
            "k": Float(0.125),
            "mad": Code(
                inputs: [(name: "a", ty: Float, node: "u"), (name: "b", ty: Float, node: "v"), (name: "c", ty: Float, node: "k")],
                output: Float,
                code: "return a * b + c;",
            ),
        },
        output: (roughness: Some("mad")),
    )"#;

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        let mut editor = MatGraphEditor::default();
        editor.new_graph(None);
        editor.doc = MatGraphDoc::new(MaterialGraph::from_ron(GRAPH).expect("graph"), BTreeMap::new());
        editor.revision += 1;
        editor.ui.selected = Some("mad".into());
        world.insert_resource(editor);
        world
    }

    fn doc_code(world: &World) -> String {
        match &world.resource::<MatGraphEditor>().doc.graph.nodes["mad"] {
            Node::Code { code, .. } => code.clone(),
            other => panic!("not a code node: {other:?}"),
        }
    }

    /// Draws the code view of `mad` alone and applies its edits, as the inspector does.
    fn frame(ctx: &egui::Context, world: &mut World, t: &mut f64, events: Vec<egui::Event>) {
        *t += 0.05;
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 700.0),
            )),
            time: Some(*t),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            let mut pending = Vec::new();
            {
                let mut editor = world.resource_mut::<MatGraphEditor>();
                let node = editor.doc.graph.nodes["mad"].clone();
                code_editor_ui(ui, &mut editor, "mad", &node, &mut pending);
            }
            apply_pending(world, pending);
        });
        output.textures_delta.clear();
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// The keyboard path: focus the code, type, Ctrl+Enter applies one undoable edit; Ctrl+F
    /// opens Find with its field focused; Esc closes it.
    #[test]
    fn the_keyboard_types_applies_and_finds() {
        let mut world = world();
        let ctx = egui::Context::default();
        let mut t = 0.0;
        frame(&ctx, &mut world, &mut t, Vec::new());
        ctx.memory_mut(|m| m.request_focus(text_id("mad")));
        frame(&ctx, &mut world, &mut t, Vec::new());
        assert!(
            ctx.memory(|m| m.has_focus(text_id("mad"))),
            "the code field takes focus"
        );
        let depth = world.resource::<crate::command::UndoStack>().undo_depth();
        frame(&ctx, &mut world, &mut t, vec![egui::Event::Text(" // x".into())]);
        assert_eq!(
            doc_code(&world),
            "return a * b + c;",
            "typing alone does not apply yet"
        );
        frame(
            &ctx,
            &mut world,
            &mut t,
            vec![key(egui::Key::Enter, egui::Modifiers::COMMAND)],
        );
        let code = doc_code(&world);
        assert!(
            code.contains("// x") && code.contains("return a * b + c;"),
            "{code:?}"
        );
        assert!(
            !code.contains('\n'),
            "Ctrl+Enter applies, it does not insert a line: {code:?}"
        );
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            depth + 1
        );
        // Ctrl+F opens Find and focuses its field; Esc closes it.
        frame(
            &ctx,
            &mut world,
            &mut t,
            vec![key(egui::Key::F, egui::Modifiers::COMMAND)],
        );
        frame(&ctx, &mut world, &mut t, Vec::new());
        let st = ctx
            .data_mut(|d| d.get_temp::<CodeEditorState>(state_id()))
            .unwrap_or_default();
        assert!(st.find_open, "Ctrl+F opens Find");
        let find = egui::Id::new(("matgraph-code-find", "mad"));
        assert!(ctx.memory(|m| m.has_focus(find)), "the Find field has focus");
        frame(&ctx, &mut world, &mut t, vec![egui::Event::Text("b".into())]);
        frame(
            &ctx,
            &mut world,
            &mut t,
            vec![key(egui::Key::Escape, egui::Modifiers::NONE)],
        );
        frame(&ctx, &mut world, &mut t, Vec::new());
        let st = ctx
            .data_mut(|d| d.get_temp::<CodeEditorState>(state_id()))
            .unwrap_or_default();
        assert!(!st.find_open, "Esc closes Find");
        assert_eq!(st.find, "b");
    }

    /// Without Ctrl+Enter the buffer applies itself after APPLY_AFTER_FRAMES frames with no
    /// keystroke, and the error list shows a syntax error at its line.
    #[test]
    fn a_pause_applies_and_errors_show_at_their_line() {
        let mut world = world();
        let ctx = egui::Context::default();
        let mut t = 0.0;
        frame(&ctx, &mut world, &mut t, Vec::new());
        ctx.memory_mut(|m| m.request_focus(text_id("mad")));
        frame(&ctx, &mut world, &mut t, Vec::new());
        frame(&ctx, &mut world, &mut t, vec![egui::Event::Text(" +".into())]);
        let mut frames = 0;
        while doc_code(&world) == "return a * b + c;" && frames < 2 * APPLY_AFTER_FRAMES {
            frame(&ctx, &mut world, &mut t, Vec::new());
            frames += 1;
        }
        assert!(
            (APPLY_AFTER_FRAMES - 2..=APPLY_AFTER_FRAMES + 2).contains(&frames),
            "applied after {frames} idle frames"
        );
        assert_eq!(doc_code(&world), "return a * b + c; +");
        let st = ctx
            .data_mut(|d| d.get_temp::<CodeEditorState>(state_id()))
            .unwrap_or_default();
        let (_, diags) = st.checked.expect("checked");
        assert!(
            diags.iter().any(|d| d.line == 1 && d.code == "code_syntax"),
            "{diags:?}"
        );
        // The compile reports the same, and the graph still previews (magenta).
        let editor = world.resource::<MatGraphEditor>();
        let resolved = editor.compile_source().expect("source");
        assert!(regia_matgraph::compile(&resolved).is_err());
        assert!(regia_matgraph::compile_lenient(&resolved).is_ok());
    }

    /// The agent methods through the editor's registered extension table.
    #[test]
    fn the_agent_sets_code_and_lists_errors_through_the_dispatcher() {
        let mut world = world();
        crate::agent_ext::install(&mut world);
        let method = |world: &World, name: &str| {
            world
                .resource::<regia_agent::extension::AgentExtension>()
                .method(name)
                .map(|(_, h)| h)
                .unwrap_or_else(|| panic!("{name} is registered"))
        };
        let set = method(&world, "editor.matgraph_code_set");
        let answer = set(
            &mut world,
            &json!({ "node": "mad", "code": "let t = a * b;\nlet u = t +;\nreturn u;" }),
        )
        .expect("set");
        assert_eq!(answer["code_ok"], json!(false));
        assert_eq!(answer["code_errors"][0]["line"], json!(2), "{answer}");
        assert_eq!(answer["code_errors"][0]["code"], json!("code_syntax"));
        assert_eq!(answer["ok"], json!(false), "the graph reports it too");
        let errors = method(&world, "editor.matgraph_code_errors");
        let list = errors(&mut world, &json!({})).expect("errors");
        assert_eq!(list["nodes"][0]["node"], json!("mad"));
        assert_eq!(list["nodes"][0]["errors"][0]["line"], json!(2));
        // Declaration and code together; wires stay by name.
        let answer = set(
            &mut world,
            &json!({
                "node": "mad",
                "inputs": [{ "name": "a", "ty": "Float" }, { "name": "c", "ty": "Float" }],
                "code": "return a + c;",
            }),
        )
        .expect("set");
        assert_eq!(answer["code_ok"], json!(true), "{answer}");
        assert_eq!(answer["ok"], json!(true), "{answer}");
        let Node::Code { inputs, .. } = &world.resource::<MatGraphEditor>().doc.graph.nodes["mad"] else {
            panic!("code node");
        };
        assert_eq!(inputs[1].node, "k", "c kept its wire");
        // Refusals carry their codes.
        let err = set(&mut world, &json!({ "node": "uv", "code": "return 1.0;" })).expect_err("not code");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("matgraph_not_code_node")
        );
        let err = set(&mut world, &json!({ "node": "nope" })).expect_err("missing");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("matgraph_node_missing")
        );
        let err = set(&mut world, &json!({ "node": "mad", "output": "Mat4" })).expect_err("type");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("matgraph_code_bad_type")
        );
        let err = errors(&mut world, &json!({ "node": "k" })).expect_err("not code");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("matgraph_not_code_node")
        );
        // Consecutive sets of one node are one undo step (like a drag): undo restores the
        // node as it was before the first.
        crate::command::undo_world(&mut world);
        assert_eq!(doc_code(&world), "return a * b + c;");
        let Node::Code { inputs, .. } = &world.resource::<MatGraphEditor>().doc.graph.nodes["mad"] else {
            panic!("code node");
        };
        assert_eq!(inputs.len(), 3);
    }

    #[test]
    fn tokens_cover_the_text_and_name_what_they_are() {
        let inputs: BTreeSet<String> = ["a".to_string()].into();
        let text = "#include \"x.wgsl\"\nlet t = a * 2.5e-1; // note\nreturn max(t, 1.0);";
        let toks = tokens(text, &inputs);
        let joined: String = toks.iter().map(|(r, _)| &text[r.clone()]).collect();
        assert_eq!(joined, text, "spans cover the text");
        let kind = |word: &str| {
            toks.iter()
                .find(|(r, _)| &text[r.clone()] == word)
                .map(|(_, t)| *t)
        };
        assert_eq!(kind("let"), Some(Token::Keyword));
        assert_eq!(kind("a"), Some(Token::Input));
        assert_eq!(kind("2.5e-1"), Some(Token::Number));
        assert_eq!(kind("max"), Some(Token::Builtin));
        assert_eq!(kind("// note"), Some(Token::Comment));
        assert_eq!(kind("#include \"x.wgsl\""), Some(Token::Directive));
        assert_eq!(kind("("), Some(Token::Bracket));
    }

    #[test]
    fn brackets_match_both_ways_and_nested() {
        let text = "f(a, (b + c)) { x[1]; }";
        assert_eq!(matching_bracket(text, 1), Some((1, 12)));
        assert_eq!(matching_bracket(text, 13), Some((12, 1)), "just after a closer");
        assert_eq!(matching_bracket(text, 5), Some((5, 11)));
        assert_eq!(matching_bracket(text, 14), Some((14, 22)));
        assert_eq!(matching_bracket("(", 0), None);
        assert_eq!(matching_bracket("abc", 1), None);
    }

    #[test]
    fn find_completion_and_lines() {
        assert_eq!(
            find_matches("Return a; return b;", "return"),
            vec![(0, 6), (10, 16)]
        );
        assert!(find_matches("abc", "").is_empty());
        let (start, word) = word_before("let t = hel", 11);
        assert_eq!((start, word.as_str()), (8, "hel"));
        let list = completions(
            "hel",
            &["helper_mad".into(), "hello".into(), "hel".into(), "x".into()],
        );
        assert_eq!(list, vec!["helper_mad".to_string(), "hello".to_string()]);
        assert!(completions("", &["a".into()]).is_empty());
        assert_eq!(line_start("a\nbc\nd", 3), 5);
        assert_eq!(line_start("a\nbc\nd", 1), 0);
        let mut text = "return mi".to_string();
        let at = insert_completion(&mut text, 7, 9, "mix");
        assert_eq!((text.as_str(), at), ("return mix", 10));
    }
}
