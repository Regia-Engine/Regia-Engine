// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask llms-txt [--check] [--out DIR]`: the book's index for AI assistants (`llms.txt`) and
//! the user-facing book in one file (`llms-full.txt`) (ADR-10135).
//!
//! Both files follow the public llms.txt proposal (<https://llmstxt.org/>): an H1, a block-quote
//! summary, then H2 sections of `- [Title](path): description` links, the last one `## Optional`.
//!
//! - **Start here**: `AGENTS.md` (the user-facing one, from the overlay), `book/start/assistant-setup.md`,
//!   `book/start/install.md`, `book/getting-started.md`.
//! - **User guide**, **Systems**, **Reference**, **Agent guide**: the chapters of `book/SUMMARY.md` in
//!   summary order, by part (Getting started, Fundamentals and Tutorials are the user guide; Engine
//!   systems is Systems; every `agents/` page outside Engine development is the agent guide).
//! - **Optional**: the Engine development part and any part this module does not know, the
//!   contributor material an assistant with a small context may skip.
//!
//! Each page is listed once (its first place wins). A title is the page's first `# ` heading and a
//! description its first sentence (the first prose paragraph's text up to the first `.`, `!` or `?`
//! that ends a sentence). `llms-full.txt` is every Start here, User guide, Systems and Agent guide
//! book page in summary order, each preceded by `<!-- source: book/... -->`, with relative links
//! rewritten to repository-root paths. `llms.txt` records `llms-full.txt`'s byte size in a comment
//! after the summary, so an assistant can decide whether to load it whole.
//!
//! The private repository keeps its root for the builders: the files are written into
//! `dist/public-root/` (the export overlay, ADR-10230), and link targets are paths relative to the
//! exported root. A link resolves against the overlay first, then the repository root. The output
//! depends only on the tree, so two runs write identical bytes. `--check` writes nothing and fails
//! when either file is stale, a linked page is missing, or a page has no title or description.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;

use crate::manual::{SummaryItem, parse_summary};

/// Where the files are written in the private repository: the public-export overlay.
pub const OVERLAY_DIR: &str = "dist/public-root";

/// The index file name.
pub const LLMS_TXT: &str = "llms.txt";

/// The full-text file name.
pub const LLMS_FULL_TXT: &str = "llms-full.txt";

/// The fixed "Start here" links, repository-root paths (ADR-10135).
pub const START_HERE: [&str; 4] = [
    "AGENTS.md",
    "book/start/assistant-setup.md",
    "book/start/install.md",
    "book/getting-started.md",
];

/// Longest description kept, in characters; a longer first sentence is cut at a word and ends in `…`.
pub const MAX_DESCRIPTION_CHARS: usize = 240;

/// Arguments of `cargo xtask llms-txt`.
#[derive(ClapArgs, Debug, Clone, Default)]
pub struct Args {
    /// Write nothing; fail when either file is stale, a linked page is missing, or a page has no
    /// title or description.
    #[arg(long)]
    pub check: bool,
    /// Directory to write into (and to resolve `AGENTS.md` in), relative to the workspace root.
    /// Default `dist/public-root`; an exported tree passes `.`.
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
}

/// The sections of `llms.txt`, in file order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    /// The fixed entry points.
    StartHere,
    /// Getting started, Fundamentals, Tutorials and the summary's prefix chapters.
    UserGuide,
    /// The Engine systems part.
    Systems,
    /// The Reference part.
    Reference,
    /// Every `agents/` page outside Engine development, and the Agent guide part.
    AgentGuide,
    /// Engine development and unknown parts: contributor material.
    Optional,
}

impl Section {
    /// The H2 heading text.
    pub fn heading(self) -> &'static str {
        match self {
            Section::StartHere => "Start here",
            Section::UserGuide => "User guide",
            Section::Systems => "Systems",
            Section::Reference => "Reference",
            Section::AgentGuide => "Agent guide",
            Section::Optional => "Optional",
        }
    }

    /// Whether the section's book pages go into `llms-full.txt`.
    pub fn in_full_text(self) -> bool {
        matches!(
            self,
            Section::StartHere | Section::UserGuide | Section::Systems | Section::AgentGuide
        )
    }

    const ALL: [Section; 6] = [
        Section::StartHere,
        Section::UserGuide,
        Section::Systems,
        Section::Reference,
        Section::AgentGuide,
        Section::Optional,
    ];
}

/// The section a summary chapter goes to, from its part title and its path relative to `book/`.
pub fn section_for(part: Option<&str>, path: &str) -> Section {
    let base = match part {
        None | Some("Getting started") | Some("Fundamentals") | Some("Tutorials") => Section::UserGuide,
        Some("Engine systems") => Section::Systems,
        Some("Reference") => Section::Reference,
        Some("Agent guide") => Section::AgentGuide,
        Some(_) => Section::Optional,
    };
    if base != Section::Optional && path.starts_with("agents/") {
        Section::AgentGuide
    } else {
        base
    }
}

/// One linked page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// Path relative to the exported root (`book/start/install.md`, `AGENTS.md`).
    pub path: String,
    /// The first `# ` heading.
    pub title: String,
    /// The first sentence.
    pub description: String,
    /// Where it is listed.
    pub section: Section,
    /// The page's markdown.
    pub text: String,
}

/// Both generated files, or the problems that stop them.
#[derive(Debug, Clone, Default)]
pub struct Generated {
    /// `llms.txt`.
    pub llms: String,
    /// `llms-full.txt`.
    pub full: String,
    /// Every linked page, in file order.
    pub pages: Vec<Page>,
    /// Missing pages, titles or descriptions; generation writes nothing while any exists.
    pub problems: Vec<String>,
}

/// Resolves a link path against the overlay first, then the repository root.
pub fn resolve(root: &Path, overlay: &Path, path: &str) -> Option<PathBuf> {
    [overlay.join(path), root.join(path)]
        .into_iter()
        .find(|p| p.is_file())
}

/// Builds both files for the tree at `root`, with the overlay directory `overlay`.
pub fn generate(root: &Path, overlay: &Path) -> Result<Generated> {
    let summary_path = root.join("book/SUMMARY.md");
    let summary = std::fs::read_to_string(&summary_path)
        .with_context(|| format!("reading {}", summary_path.display()))?;
    let mut out = Generated::default();
    let mut listed: Vec<(String, Section)> = START_HERE
        .iter()
        .map(|p| ((*p).to_string(), Section::StartHere))
        .collect();
    let mut seen: BTreeSet<String> = START_HERE.iter().map(|p| (*p).to_string()).collect();
    let mut part: Option<String> = None;
    for item in parse_summary(&summary) {
        match item {
            SummaryItem::Part(title) => part = Some(title),
            SummaryItem::Chapter { path, .. } => {
                // Chapters outside the book (`../docs/...`) are builder documents, not the manual.
                if path.starts_with("../") || path.contains("://") {
                    continue;
                }
                let full = format!("book/{}", path.split('#').next().unwrap_or(&path));
                if seen.insert(full.clone()) {
                    listed.push((full, section_for(part.as_deref(), &path)));
                }
            }
        }
    }
    for (path, section) in listed {
        let Some(file) = resolve(root, overlay, &path) else {
            out.problems.push(format!("{path}: linked page does not exist"));
            continue;
        };
        let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let title = page_title(&text);
        let description = first_sentence(&text);
        if title.is_none() {
            out.problems.push(format!("{path}: no `# ` title"));
        }
        if description.is_none() {
            out.problems.push(format!(
                "{path}: no description (no prose paragraph to take a first sentence from)"
            ));
        }
        out.pages.push(Page {
            path,
            title: title.unwrap_or_default(),
            description: description.unwrap_or_default(),
            section,
            text,
        });
    }
    let summary_text = resolve(root, overlay, "AGENTS.md")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| first_paragraph(&t))
        .unwrap_or_default();
    if summary_text.is_empty() {
        out.problems
            .push("AGENTS.md: no first paragraph for the summary".into());
    }
    out.full = full_text(&out.pages);
    out.llms = index_text(&out.pages, &summary_text, out.full.len());
    Ok(out)
}

/// `llms.txt` from the pages, the summary paragraph and `llms-full.txt`'s byte size.
fn index_text(pages: &[Page], summary: &str, full_bytes: usize) -> String {
    let full_pages = pages
        .iter()
        .filter(|p| p.section.in_full_text() && p.path.starts_with("book/"))
        .count();
    let mut s = String::from("# Regia Engine\n\n");
    s.push_str(&format!("> {summary}\n\n"));
    s.push_str(&format!(
        "<!-- Generated by `cargo xtask llms-txt`; do not edit. llms-full.txt: {full_bytes} bytes, {full_pages} pages. -->\n\n"
    ));
    s.push_str(
        "Paths are relative to the repository root. If you were asked to set Regia up, read `AGENTS.md` first. \
         `llms-full.txt` holds the Start here, User guide, Systems and Agent guide pages in one file.\n",
    );
    for section in Section::ALL {
        let rows: Vec<&Page> = pages.iter().filter(|p| p.section == section).collect();
        if rows.is_empty() {
            continue;
        }
        s.push_str(&format!("\n## {}\n\n", section.heading()));
        for page in rows {
            s.push_str(&format!(
                "- [{}]({}): {}\n",
                escape_link_text(&page.title),
                page.path,
                page.description
            ));
        }
    }
    s
}

/// `llms-full.txt`: the full-text pages in order, each after a source comment.
fn full_text(pages: &[Page]) -> String {
    let mut s = String::new();
    let mut ordered: Vec<&Page> = pages
        .iter()
        .filter(|p| p.section.in_full_text() && p.path.starts_with("book/"))
        .collect();
    // Start here pages first in their fixed order, then the rest in summary order (`pages` order).
    ordered.sort_by_key(|p| p.section != Section::StartHere);
    for page in ordered {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(&format!("<!-- source: {} -->\n", page.path));
        let body = rewrite_links(&page.text, &page.path);
        s.push_str(body.trim_end());
        s.push('\n');
    }
    s
}

/// Escapes `[`, `]` and `\` in link text.
fn escape_link_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// The text of the first `# ` heading outside code fences.
pub fn page_title(text: &str) -> Option<String> {
    let mut fence = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        if let Some(title) = line.strip_prefix("# ") {
            let title = title.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

/// Whether a line cannot start a prose paragraph (headings, tables, lists, quotes, HTML, rules,
/// images, front matter).
fn is_structural(t: &str) -> bool {
    t.starts_with('#')
        || t.starts_with('|')
        || t.starts_with('>')
        || t.starts_with('<')
        || t.starts_with("![")
        || t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("+ ")
        || t.starts_with("---")
        || t.starts_with("***")
        || t.starts_with(":::")
        || t.starts_with("[^")
        || t.starts_with("[!")
        || t.split_once(". ")
            .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// A paragraph wholly in emphasis (`*Prerequisites: ...*`) is a note, not the page's opening.
fn is_emphasis_note(p: &str) -> bool {
    let p = p.trim();
    p.len() > 2
        && ((p.starts_with('*') && p.ends_with('*') && !p.starts_with("**"))
            || (p.starts_with('_') && p.ends_with('_')))
}

/// The first prose paragraph outside code fences, its lines joined with single spaces.
pub fn first_paragraph(text: &str) -> Option<String> {
    let mut fence = false;
    let mut para: Vec<&str> = Vec::new();
    let mut lines = text.lines().peekable();
    // Skip a YAML front-matter block.
    if lines.peek().map(|l| l.trim()) == Some("---") {
        lines.next();
        for line in lines.by_ref() {
            if line.trim() == "---" {
                break;
            }
        }
    }
    for line in lines {
        let t = line.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            if !para.is_empty() {
                let joined = para.join(" ");
                if !is_emphasis_note(&joined) {
                    return Some(joined);
                }
                para.clear();
            }
            continue;
        }
        if fence {
            continue;
        }
        if t.is_empty()
            || (para.is_empty() && is_structural(t))
            || (!para.is_empty() && (t.starts_with('#') || t.starts_with('|')))
        {
            if !para.is_empty() {
                let joined = para.join(" ");
                if !is_emphasis_note(&joined) {
                    return Some(joined);
                }
                para.clear();
            }
            continue;
        }
        para.push(t);
    }
    let joined = para.join(" ");
    (!joined.is_empty() && !is_emphasis_note(&joined)).then_some(joined)
}

/// Replaces `[text](target)` and `![alt](target)` by their text, and drops `**` bold marks.
pub fn strip_links(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut code = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            code = !code;
            out.push(c);
            i += 1;
            continue;
        }
        if !code && c == '*' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            continue;
        }
        let image = !code && c == '!' && chars.get(i + 1) == Some(&'[');
        if !code && (c == '[' || image) {
            let open = if image { i + 1 } else { i };
            if let Some(close) = matching(&chars, open, '[', ']')
                && chars.get(close + 1) == Some(&'(')
                && let Some(end) = matching(&chars, close + 1, '(', ')')
            {
                let inner: String = chars[open + 1..close].iter().collect();
                out.push_str(&strip_links(&inner));
                i = end + 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// The index of the bracket closing the one at `open`, counting nesting.
fn matching(chars: &[char], open: usize, l: char, r: char) -> Option<usize> {
    let mut depth = 0usize;
    for (k, &c) in chars.iter().enumerate().skip(open) {
        if c == l {
            depth += 1;
        } else if c == r {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(k);
            }
        }
    }
    None
}

/// Words before a `.` that do not end a sentence.
const ABBREVIATIONS: [&str; 6] = ["e.g", "i.e", "vs", "cf", "etc", "approx"];

/// The first sentence of the first prose paragraph, links reduced to their text, cut to
/// [`MAX_DESCRIPTION_CHARS`].
pub fn first_sentence(text: &str) -> Option<String> {
    let para = strip_links(&first_paragraph(text)?);
    let chars: Vec<char> = para.chars().collect();
    let mut code = false;
    let mut depth = 0i32;
    let mut end = chars.len();
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '`' => code = !code,
            '(' | '[' if !code => depth += 1,
            ')' | ']' if !code => depth -= 1,
            '.' | '!' | '?' if !code && depth <= 0 => {
                // Closing quotes and emphasis marks after the stop belong to the sentence: `."*` ends it.
                let mut after = i + 1;
                while chars
                    .get(after)
                    .is_some_and(|c| matches!(c, '"' | '\'' | '*' | '_' | '”' | '’'))
                {
                    after += 1;
                }
                let next_ends = chars.get(after).is_none_or(|n| n.is_whitespace());
                if !next_ends {
                    continue;
                }
                if c == '.' {
                    let word: String = chars[..i]
                        .iter()
                        .rev()
                        .take_while(|ch| !ch.is_whitespace() && **ch != '(')
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    if ABBREVIATIONS.contains(&word.to_ascii_lowercase().as_str()) {
                        continue;
                    }
                }
                end = after;
                break;
            }
            _ => {}
        }
    }
    let sentence: String = chars[..end].iter().collect();
    let sentence = sentence.split_whitespace().collect::<Vec<_>>().join(" ");
    if sentence.is_empty() {
        return None;
    }
    if sentence.chars().count() <= MAX_DESCRIPTION_CHARS {
        return Some(sentence);
    }
    let cut: String = sentence.chars().take(MAX_DESCRIPTION_CHARS).collect();
    let cut = match cut.rfind(' ') {
        Some(at) if at > 0 => cut[..at].trim_end_matches([',', ';', ':']).to_string(),
        _ => cut,
    };
    Some(format!("{cut}…"))
}

/// Normalizes `a/b/../c` to `a/c`; `None` when it climbs above the root.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

/// Rewrites a link target relative to `page` (a repository-root path) to a repository-root path.
/// External links, in-page anchors and absolute paths are kept.
pub fn rewrite_target(target: &str, page: &str) -> String {
    let t = target.trim();
    if t.is_empty()
        || t.starts_with('#')
        || t.starts_with('/')
        || t.contains("://")
        || t.starts_with("mailto:")
    {
        return target.to_string();
    }
    let (path, frag) = match t.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (t, None),
    };
    let dir = page.rsplit_once('/').map_or("", |(d, _)| d);
    let joined = if dir.is_empty() {
        path.to_string()
    } else {
        format!("{dir}/{path}")
    };
    match normalize(&joined) {
        Some(n) => match frag {
            Some(f) => format!("{n}#{f}"),
            None => n,
        },
        None => target.to_string(),
    }
}

/// Rewrites every inline link target of `text` (outside code) with [`rewrite_target`].
pub fn rewrite_links(text: &str, page: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fence = false;
    for line in text.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            out.push_str(line);
            continue;
        }
        if fence {
            out.push_str(line);
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut code = false;
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '`' {
                code = !code;
            }
            if !code
                && c == ']'
                && chars.get(i + 1) == Some(&'(')
                && let Some(end) = matching(&chars, i + 1, '(', ')')
            {
                let inner: String = chars[i + 2..end].iter().collect();
                // `(target "title")`: rewrite the target only.
                let (target, rest) = match inner.split_once(' ') {
                    Some((t, r)) => (t.to_string(), format!(" {r}")),
                    None => (inner.clone(), String::new()),
                };
                out.push_str("](");
                out.push_str(&rewrite_target(&target, page));
                out.push_str(&rest);
                out.push(')');
                i = end + 1;
                continue;
            }
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Writes `text` to `path` through a temporary file and a rename.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Generates, then writes or (with `check`) compares. Returns the problems found; empty means the
/// files are (or were made) current.
pub fn check_or_write(root: &Path, out_dir: &Path, check: bool) -> Result<Vec<String>> {
    let generated = generate(root, out_dir)?;
    let mut problems = generated.problems.clone();
    if !problems.is_empty() {
        return Ok(problems);
    }
    for (name, text) in [(LLMS_TXT, &generated.llms), (LLMS_FULL_TXT, &generated.full)] {
        let path = out_dir.join(name);
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current == *text {
            continue;
        }
        if check {
            problems.push(format!("{} is stale: run `cargo xtask llms-txt`", path.display()));
        } else {
            write_atomic(&path, text)?;
        }
    }
    Ok(problems)
}

/// Runs `cargo xtask llms-txt`.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out_dir = root.join(args.out.clone().unwrap_or_else(|| PathBuf::from(OVERLAY_DIR)));
    let problems = check_or_write(root, &out_dir, args.check)?;
    if problems.is_empty() {
        println!(
            "llms-txt: {} {} and {}",
            if args.check { "current:" } else { "wrote" },
            out_dir.join(LLMS_TXT).display(),
            out_dir.join(LLMS_FULL_TXT).display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    for p in &problems {
        eprintln!("llms-txt: {p}");
    }
    eprintln!("llms-txt: {} problem(s)", problems.len());
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENTS: &str = "<!-- regia:user-agents-md -->\n# Regia Engine: instructions for AI assistants\n\n\
        Regia Engine is a game engine. Games are Rust projects.\n\n## Rules\n";

    /// A small book: a summary with a prefix chapter, three parts and a page outside the book.
    fn fixture() -> (PathBuf, PathBuf) {
        let root = regia_test_utils::unique_temp_dir("llms_txt");
        let overlay = root.join(OVERLAY_DIR);
        let files: &[(&str, &str)] = &[
            (
                "book/SUMMARY.md",
                "# Summary\n\n[Introduction](introduction.md)\n\n# Getting started\n\n\
                 - [Installing](start/install.md)\n- [Assistant setup](start/assistant-setup.md)\n\
                 - [First project](getting-started.md)\n- [Quick start](user-guide/quick-start.md)\n\
                 # Engine systems\n\n- [Rendering](user-guide/rendering.md)\n  - [Install again](start/install.md)\n\n\
                 # Reference\n\n- [CLI](reference/cli.md)\n- [Format change](../docs/formats/x.md)\n\n\
                 # Agent guide\n\n- [Protocol](agents/protocol.md)\n\n# Engine development\n\n\
                 - [Merge gate](user-guide/merge-gate.md)\n- [Dev agent page](agents/dev.md)\n",
            ),
            (
                "book/introduction.md",
                "# Introduction\n\nThe manual. It covers everything.\n",
            ),
            (
                "book/start/install.md",
                "# Installing Regia Engine\n\nYou build the tools from source, e.g. with cargo. Then more.\n\n\
                 See [the guide](../user-guide/rendering.md#a) and [web](https://example.org).\n",
            ),
            (
                "book/start/assistant-setup.md",
                "# Set up with your AI assistant\n\n> a quote\n\nAsk your assistant to [set it up](install.md). Done.\n",
            ),
            (
                "book/getting-started.md",
                "# Your first project\n\nThis chapter creates a project.\n",
            ),
            (
                "book/user-guide/quick-start.md",
                "# Quick start\n\n*Prerequisites: a checkout. Reading time: 8 minutes.*\n\nOne file is a game. More.\n",
            ),
            (
                "book/user-guide/rendering.md",
                "# Rendering\n\n```rust\nfn main() {}\n```\n\nThe renderer (Vulkan, DX12. Metal) draws frames! Yes.\n",
            ),
            (
                "book/reference/cli.md",
                "# Command-line tools\n\nEvery `regia` command.\n",
            ),
            (
                "book/agents/protocol.md",
                "# Protocol\n\n| a | b |\n\nThe protocol is JSON-RPC.\n",
            ),
            (
                "book/user-guide/merge-gate.md",
                "# The merge gate\n\nFor contributors.\n",
            ),
            ("book/agents/dev.md", "# Dev agent page\n\nA contributor page.\n"),
        ];
        for (path, text) in files {
            let p = root.join(path);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(&p, text).expect("write");
        }
        std::fs::create_dir_all(&overlay).expect("overlay");
        std::fs::write(overlay.join("AGENTS.md"), AGENTS).expect("agents");
        (root, overlay)
    }

    #[test]
    fn sections_follow_the_parts() {
        assert_eq!(section_for(None, "introduction.md"), Section::UserGuide);
        assert_eq!(
            section_for(Some("Tutorials"), "tutorials/a.md"),
            Section::UserGuide
        );
        assert_eq!(
            section_for(Some("Engine systems"), "user-guide/a.md"),
            Section::Systems
        );
        assert_eq!(
            section_for(Some("Engine systems"), "agents/a.md"),
            Section::AgentGuide
        );
        assert_eq!(
            section_for(Some("Engine development"), "agents/a.md"),
            Section::Optional
        );
        assert_eq!(section_for(Some("A new part"), "x.md"), Section::Optional);
    }

    #[test]
    fn first_sentence_skips_notes_code_and_abbreviations() {
        assert_eq!(
            first_sentence("# T\n\nYou build it, e.g. with cargo. Then more.\n").as_deref(),
            Some("You build it, e.g. with cargo.")
        );
        assert_eq!(
            first_sentence("# T\n\n*Prerequisites: x. Time: 8 min.*\n\nOne file. More.\n").as_deref(),
            Some("One file.")
        );
        assert_eq!(
            first_sentence("# T\n\n```\nnot this.\n```\n\nThe renderer (Vulkan. DX12) draws! Yes.\n")
                .as_deref(),
            Some("The renderer (Vulkan. DX12) draws!")
        );
        assert_eq!(
            first_sentence("# T\n\nRead [the `a.b` page](x.md) and **this**. Next.\n").as_deref(),
            Some("Read the `a.b` page and this.")
        );
        assert_eq!(
            first_sentence("# T\n\nSay *\"Set it up.\"* Then more.\n").as_deref(),
            Some("Say *\"Set it up.\"*")
        );
        assert_eq!(first_sentence("# T\n\n| a | b |\n\n## H\n"), None);
        let long = format!("# T\n\n{} end.\n", "word ".repeat(80));
        let d = first_sentence(&long).expect("description");
        assert!(
            d.ends_with('…') && d.chars().count() <= MAX_DESCRIPTION_CHARS + 1,
            "{d}"
        );
    }

    #[test]
    fn links_are_rewritten_to_root_paths() {
        assert_eq!(
            rewrite_target("../user-guide/a.md#x", "book/start/install.md"),
            "book/user-guide/a.md#x"
        );
        assert_eq!(
            rewrite_target("install.md", "book/start/a.md"),
            "book/start/install.md"
        );
        assert_eq!(rewrite_target("#anchor", "book/a.md"), "#anchor");
        assert_eq!(rewrite_target("https://x.org/a", "book/a.md"), "https://x.org/a");
        let text = "See [a](../b.md) and `[c](d.md)`.\n```\n[e](f.md)\n```\n![img](i.png \"t\")\n";
        assert_eq!(
            rewrite_links(text, "book/start/p.md"),
            "See [a](book/b.md) and `[c](d.md)`.\n```\n[e](f.md)\n```\n![img](book/start/i.png \"t\")\n"
        );
    }

    #[test]
    fn generation_lists_each_page_once_in_its_section() {
        let (root, overlay) = fixture();
        let g = generate(&root, &overlay).expect("generate");
        assert!(g.problems.is_empty(), "{:?}", g.problems);
        let expected = format!(
            "# Regia Engine\n\n> Regia Engine is a game engine. Games are Rust projects.\n\n\
             <!-- Generated by `cargo xtask llms-txt`; do not edit. llms-full.txt: {} bytes, 7 pages. -->\n\n\
             Paths are relative to the repository root. If you were asked to set Regia up, read `AGENTS.md` first. \
             `llms-full.txt` holds the Start here, User guide, Systems and Agent guide pages in one file.\n\n\
             ## Start here\n\n\
             - [Regia Engine: instructions for AI assistants](AGENTS.md): Regia Engine is a game engine.\n\
             - [Set up with your AI assistant](book/start/assistant-setup.md): Ask your assistant to set it up.\n\
             - [Installing Regia Engine](book/start/install.md): You build the tools from source, e.g. with cargo.\n\
             - [Your first project](book/getting-started.md): This chapter creates a project.\n\n\
             ## User guide\n\n\
             - [Introduction](book/introduction.md): The manual.\n\
             - [Quick start](book/user-guide/quick-start.md): One file is a game.\n\n\
             ## Systems\n\n\
             - [Rendering](book/user-guide/rendering.md): The renderer (Vulkan, DX12. Metal) draws frames!\n\n\
             ## Reference\n\n\
             - [Command-line tools](book/reference/cli.md): Every `regia` command.\n\n\
             ## Agent guide\n\n\
             - [Protocol](book/agents/protocol.md): The protocol is JSON-RPC.\n\n\
             ## Optional\n\n\
             - [The merge gate](book/user-guide/merge-gate.md): For contributors.\n\
             - [Dev agent page](book/agents/dev.md): A contributor page.\n",
            g.full.len()
        );
        assert_eq!(g.llms, expected);
        // The full text: Start here book pages first, then summary order; no reference, no optional.
        let sources: Vec<&str> = g
            .full
            .lines()
            .filter_map(|l| l.strip_prefix("<!-- source: ")?.strip_suffix(" -->"))
            .collect();
        assert_eq!(
            sources,
            [
                "book/start/assistant-setup.md",
                "book/start/install.md",
                "book/getting-started.md",
                "book/introduction.md",
                "book/user-guide/quick-start.md",
                "book/user-guide/rendering.md",
                "book/agents/protocol.md"
            ]
        );
        assert!(
            g.full.contains("[the guide](book/user-guide/rendering.md#a)"),
            "{}",
            g.full
        );
        assert!(g.full.contains("[set it up](book/start/install.md)"));
    }

    #[test]
    fn every_index_link_resolves() {
        let (root, overlay) = fixture();
        let g = generate(&root, &overlay).expect("generate");
        let mut links = 0;
        for line in g.llms.lines() {
            let Some(rest) = line.strip_prefix("- [") else {
                continue;
            };
            let target = rest
                .split("](")
                .nth(1)
                .and_then(|r| r.split(')').next())
                .expect("target");
            assert!(
                resolve(&root, &overlay, target).is_some(),
                "{target} does not resolve"
            );
            links += 1;
        }
        assert_eq!(links, 11);
    }

    #[test]
    fn check_passes_after_generation_and_fails_after_a_title_change() {
        let (root, overlay) = fixture();
        // Nothing generated yet: stale.
        let before = check_or_write(&root, &overlay, true).expect("check");
        assert_eq!(before.len(), 2, "{before:?}");
        assert!(check_or_write(&root, &overlay, false).expect("write").is_empty());
        assert!(check_or_write(&root, &overlay, true).expect("check").is_empty());
        // Deterministic: a second write changes nothing.
        let first = std::fs::read(overlay.join(LLMS_TXT)).expect("read");
        assert!(check_or_write(&root, &overlay, false).expect("write").is_empty());
        assert_eq!(std::fs::read(overlay.join(LLMS_TXT)).expect("read"), first);
        // Touch a page title: both files are stale.
        let page = root.join("book/reference/cli.md");
        std::fs::write(&page, "# Command-line tools (all)\n\nEvery `regia` command.\n").expect("write");
        let after = check_or_write(&root, &overlay, true).expect("check");
        assert_eq!(after.len(), 1, "{after:?}");
        assert!(after[0].contains("llms.txt is stale"), "{after:?}");
        // A body change reaches only the full text when the page is in it.
        assert!(check_or_write(&root, &overlay, false).expect("write").is_empty());
        let install = root.join("book/start/install.md");
        let text = std::fs::read_to_string(&install).expect("read") + "\nA new line.\n";
        std::fs::write(&install, text).expect("write");
        let after = check_or_write(&root, &overlay, true).expect("check");
        assert_eq!(after.len(), 2, "the size comment moves too: {after:?}");
    }

    #[test]
    fn missing_pages_titles_and_descriptions_fail() {
        let (root, overlay) = fixture();
        std::fs::remove_file(root.join("book/agents/protocol.md")).expect("rm");
        std::fs::write(root.join("book/reference/cli.md"), "No title here.\n").expect("write");
        std::fs::write(
            root.join("book/getting-started.md"),
            "# Your first project\n\n| only | a table |\n",
        )
        .expect("write");
        let problems = check_or_write(&root, &overlay, false).expect("run");
        assert_eq!(
            problems,
            [
                "book/getting-started.md: no description (no prose paragraph to take a first sentence from)",
                "book/reference/cli.md: no `# ` title",
                "book/agents/protocol.md: linked page does not exist",
            ]
        );
        assert!(
            !overlay.join(LLMS_TXT).exists(),
            "nothing is written while a problem exists"
        );
    }
}
