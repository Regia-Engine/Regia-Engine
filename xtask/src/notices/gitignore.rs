// SPDX-License-Identifier: MPL-2.0
//! A small `.gitignore` reader for the no-`.git` walk of [`super::tracked_assets`] (ADR-7570).
//!
//! A source export (`git archive`, the merge gate's staged tree) has no `.git`, so the asset list
//! comes from a walk of the tree. Tests and sample cooks write untracked output into that tree
//! (`samples/*/build/paks/`, the fetched `samples/third_person/assets/Fox.glb`), and every such
//! path is ignored by the repository's own `.gitignore` files. Honouring those files makes the walk
//! list what `git ls-files` lists in a clean checkout.
//!
//! Pattern semantics follow the public gitignore(5) manual page (git-scm.com/docs/gitignore):
//! blank lines and `#` comments are skipped; `\` escapes the next character; unescaped trailing
//! spaces are dropped; `!` re-includes; a trailing `/` matches directories only; a pattern with a
//! `/` at its start or in its middle is relative to the directory of its `.gitignore`, otherwise
//! it matches at any depth below it; `*` and `?` never match `/`; `[...]` is a character class
//! (`!` or `^` negates it); `**/` matches zero or more directories and a trailing `/**` everything
//! inside. The last matching rule wins, and a deeper `.gitignore` is consulted after a shallower
//! one. A path inside an ignored directory stays ignored (the walk never enters the directory).

use std::path::Path;

/// One pattern line of a `.gitignore`.
#[derive(Debug, Clone)]
struct Rule {
    /// Path components to match, relative to the `.gitignore`'s directory; `**` is a component.
    parts: Vec<String>,
    /// `!pattern`: a match re-includes the path.
    negate: bool,
    /// `pattern/`: matches directories only.
    dir_only: bool,
}

/// The rules of one `.gitignore`, with the directory they apply under.
#[derive(Debug, Clone)]
pub(super) struct IgnoreFile {
    /// Directory of the `.gitignore`, relative to the walk root, `/`-separated (`""` at the root).
    base: String,
    rules: Vec<Rule>,
}

impl IgnoreFile {
    /// Parses `.gitignore` text that lives in `base` (relative to the walk root).
    pub(super) fn parse(base: &str, text: &str) -> Self {
        let rules = text.lines().filter_map(parse_line).collect();
        Self {
            base: base.trim_matches('/').to_string(),
            rules,
        }
    }

    /// Reads `<dir>/.gitignore` if there is one; `base` is `dir` relative to the walk root.
    pub(super) fn read(dir: &Path, base: &str) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join(".gitignore")).ok()?;
        Some(Self::parse(base, &text))
    }

    /// `Some(true)` if the last rule of this file that matches `rel` ignores it, `Some(false)` if it
    /// re-includes it, `None` if no rule matches. `rel` is relative to the walk root.
    fn verdict(&self, rel: &str, is_dir: bool) -> Option<bool> {
        let local = if self.base.is_empty() {
            rel
        } else {
            rel.strip_prefix(self.base.as_str())?.strip_prefix('/')?
        };
        let path: Vec<&str> = local.split('/').collect();
        self.rules
            .iter()
            .rev()
            .find(|r| (!r.dir_only || is_dir) && match_parts(&r.parts, &path))
            .map(|r| !r.negate)
    }
}

/// Whether `rel` (relative to the walk root, `/`-separated) is ignored by `files`, which are
/// ordered from the root down: a deeper file's verdict overrides a shallower one's.
pub(super) fn is_ignored(files: &[IgnoreFile], rel: &str, is_dir: bool) -> bool {
    files
        .iter()
        .rev()
        .find_map(|f| f.verdict(rel, is_dir))
        .unwrap_or(false)
}

fn parse_line(line: &str) -> Option<Rule> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    // Drop unescaped trailing spaces.
    let mut end = line.len();
    while end > 0 && line.as_bytes()[end - 1] == b' ' {
        let backslashes = line.as_bytes()[..end - 1]
            .iter()
            .rev()
            .take_while(|&&b| b == b'\\')
            .count();
        if !backslashes.is_multiple_of(2) {
            break;
        }
        end -= 1;
    }
    let mut pat = &line[..end];
    let negate = pat.starts_with('!');
    if negate {
        pat = &pat[1..];
    }
    let dir_only = pat.ends_with('/') && !pat.ends_with("\\/");
    let pat = pat.trim_end_matches('/');
    if pat.is_empty() {
        return None;
    }
    // A `/` at the start or in the middle anchors the pattern to the `.gitignore`'s directory.
    let anchored = pat.contains('/');
    let pat = pat.strip_prefix('/').unwrap_or(pat);
    let mut parts: Vec<String> = Vec::new();
    if !anchored {
        parts.push("**".into());
    }
    parts.extend(pat.split('/').filter(|p| !p.is_empty()).map(str::to_string));
    Some(Rule {
        parts,
        negate,
        dir_only,
    })
}

/// Component-wise match: `**` matches zero or more components (one or more when it is the last
/// pattern component, so `a/**` matches what is inside `a` but not `a` itself).
fn match_parts(pat: &[String], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((first, rest)) if first == "**" => {
            let min = usize::from(rest.is_empty());
            (min..=path.len()).any(|skip| match_parts(rest, &path[skip..]))
        }
        Some((first, rest)) => path.split_first().is_some_and(|(p, prest)| {
            match_component(first.as_bytes(), p.as_bytes()) && match_parts(rest, prest)
        }),
    }
}

/// `fnmatch` within one path component: `*`, `?`, `[...]` and `\` escapes.
fn match_component(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') => (0..=s.len()).any(|i| match_component(&p[1..], &s[i..])),
        Some(b'?') => !s.is_empty() && match_component(&p[1..], &s[1..]),
        Some(b'[') => match (s.first(), class(&p[1..])) {
            (Some(&c), Some((set, len))) => set.matches(c) && match_component(&p[1 + len..], &s[1..]),
            // An unclosed `[` is a literal.
            (Some(&c), None) => c == b'[' && match_component(&p[1..], &s[1..]),
            (None, _) => false,
        },
        Some(b'\\') if p.len() > 1 => s.first() == Some(&p[1]) && match_component(&p[2..], &s[1..]),
        Some(&c) => s.first() == Some(&c) && match_component(&p[1..], &s[1..]),
    }
}

/// A parsed `[...]` class.
struct Class {
    negate: bool,
    ranges: Vec<(u8, u8)>,
}

impl Class {
    fn matches(&self, c: u8) -> bool {
        self.ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&c)) != self.negate
    }
}

/// Parses the class after its `[`; returns it and the bytes it used including the `]`.
fn class(p: &[u8]) -> Option<(Class, usize)> {
    let mut i = 0;
    let negate = matches!(p.first(), Some(b'!' | b'^'));
    if negate {
        i += 1;
    }
    let mut ranges = Vec::new();
    let mut first = true;
    loop {
        let mut c = *p.get(i)?;
        if c == b']' && !first {
            return Some((Class { negate, ranges }, i + 1));
        }
        first = false;
        if c == b'\\' {
            i += 1;
            c = *p.get(i)?;
        }
        if p.get(i + 1) == Some(&b'-') && p.get(i + 2).is_some_and(|&h| h != b']') {
            ranges.push((c, p[i + 2]));
            i += 3;
        } else {
            ranges.push((c, c));
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignored(files: &[IgnoreFile], rel: &str) -> bool {
        is_ignored(files, rel, false)
    }

    /// The repository's root rules that the v050rc3 gate tree tripped over.
    #[test]
    fn the_root_rules_ignore_build_output_at_any_depth() {
        let root = IgnoreFile::parse(
            "",
            "/target\n**/.regia/\n**/build/\n*.keystore\n/crates/*/target/\n",
        );
        let files = [root];
        assert!(is_ignored(&files, "samples/arena/build", true));
        assert!(!is_ignored(&files, "samples/arena/build", false), "dir-only");
        assert!(is_ignored(&files, "build", true));
        assert!(is_ignored(&files, "target", true));
        assert!(
            !is_ignored(&files, "samples/x/target", true),
            "`/target` is anchored"
        );
        assert!(is_ignored(&files, "crates/c/target", true));
        assert!(
            !is_ignored(&files, "crates/c/d/target", true),
            "`*` does not cross `/`"
        );
        assert!(ignored(&files, "a/b/release.keystore"));
        assert!(is_ignored(&files, "samples/s/.regia", true));
        assert!(!ignored(&files, "samples/s/builder.rs"));
    }

    #[test]
    fn nested_files_anchor_to_their_directory_and_override_the_root() {
        let files = [
            IgnoreFile::parse("", "*.log\n"),
            IgnoreFile::parse("samples/third_person", "# fetched\nassets/Fox.glb\n!keep.log\n"),
        ];
        assert!(ignored(&files, "samples/third_person/assets/Fox.glb"));
        assert!(
            !ignored(&files, "samples/third_person/x/assets/Fox.glb"),
            "anchored"
        );
        assert!(
            !ignored(&files, "samples/other/assets/Fox.glb"),
            "another directory"
        );
        assert!(ignored(&files, "samples/third_person/a.log"));
        assert!(
            !ignored(&files, "samples/third_person/keep.log"),
            "the deeper file re-includes"
        );
        assert!(
            ignored(&files, "keep.log"),
            "the deeper file does not apply above itself"
        );
    }

    #[test]
    fn last_rule_wins_and_negation_reincludes() {
        let files = [IgnoreFile::parse(
            "ev",
            "*\n!.gitignore\n!README.md\n!*_sheet.jpg\n",
        )];
        assert!(ignored(&files, "ev/a.png"));
        assert!(!ignored(&files, "ev/README.md"));
        assert!(!ignored(&files, "ev/x_sheet.jpg"));
        assert!(!ignored(&files, "other/a.png"));
    }

    #[test]
    fn wildcards_classes_escapes_and_double_stars() {
        let files = [IgnoreFile::parse(
            "",
            "a/**/z\nlogs/**\n[abc]?.tmp\nf[!0-9].x\n\\#hash\n\\!bang\ntrail\\ \nspace   \n/target-*\n",
        )];
        assert!(ignored(&files, "a/z"), "`**/` matches zero directories");
        assert!(ignored(&files, "a/b/c/z"));
        assert!(ignored(&files, "logs/x/y"));
        assert!(!is_ignored(&files, "logs", true), "`a/**` is what is inside `a`");
        assert!(ignored(&files, "d/b1.tmp"));
        assert!(!ignored(&files, "d/d1.tmp"));
        assert!(ignored(&files, "fa.x"));
        assert!(!ignored(&files, "f1.x"));
        assert!(ignored(&files, "#hash"));
        assert!(ignored(&files, "!bang"));
        assert!(ignored(&files, "trail "), "an escaped trailing space is kept");
        assert!(ignored(&files, "space"), "unescaped trailing spaces are dropped");
        assert!(ignored(&files, "target-x"));
        assert!(!ignored(&files, "s/target-x"));
    }

    #[test]
    fn comments_and_blank_lines_are_not_rules() {
        let f = IgnoreFile::parse("", "# a comment\n\n   \n\r\n/\n!\n");
        assert!(f.rules.is_empty(), "{:?}", f.rules);
        assert!(!ignored(&[IgnoreFile::parse("", "# x\n\n")], "x"));
    }
}
