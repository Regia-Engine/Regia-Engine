// SPDX-License-Identifier: MPL-2.0
//! Tag rules (`config/asset_tags.ron`, format `asset_tags` v1) and the automatic tags every
//! registry record carries (`docs/specs/content-data.md` § 2.1).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::RegistryError;

/// Current `asset_tags` format version.
pub const TAG_RULES_FORMAT_VERSION: u32 = 1;

/// Project-relative path of the tag rules file.
pub const TAG_RULES_PATH: &str = "config/asset_tags.ron";

/// Longest tag key or value accepted, in bytes.
pub const MAX_TAG_LEN: usize = 256;

/// One rule: every asset whose source path matches one of `paths` gets `tags`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagRule {
    /// Path globs over project-relative source paths (`assets/props/**`); `*` stays inside one
    /// folder, `**` crosses folders, `?` is one character.
    pub paths: Vec<String>,
    /// Tags to set, key → value.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

/// The tag rules file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagRules {
    /// Must be [`TAG_RULES_FORMAT_VERSION`].
    pub format_version: u32,
    /// Rules in file order; later rules override earlier ones key by key.
    #[serde(default)]
    pub rules: Vec<TagRule>,
}

impl Default for TagRules {
    fn default() -> Self {
        Self {
            format_version: TAG_RULES_FORMAT_VERSION,
            rules: Vec::new(),
        }
    }
}

impl TagRules {
    /// Parses a rules file. Unknown versions, unknown fields and bad tag keys are errors.
    ///
    /// # Errors
    ///
    /// [`RegistryError::TagRules`] naming the problem.
    pub fn from_ron(text: &str) -> Result<Self, RegistryError> {
        let rules: Self = ron::from_str(text).map_err(|e| RegistryError::TagRules(e.to_string()))?;
        if rules.format_version != TAG_RULES_FORMAT_VERSION {
            return Err(RegistryError::TagRules(format!(
                "format_version {} is not supported (expected {TAG_RULES_FORMAT_VERSION})",
                rules.format_version
            )));
        }
        for (i, rule) in rules.rules.iter().enumerate() {
            if rule.paths.is_empty() {
                return Err(RegistryError::TagRules(format!("rule {i} has no paths")));
            }
            for (key, value) in &rule.tags {
                if let Some(problem) = tag_key_problem(key) {
                    return Err(RegistryError::TagRules(format!(
                        "rule {i}: tag key {key:?} {problem}"
                    )));
                }
                if value.len() > MAX_TAG_LEN {
                    return Err(RegistryError::TagRules(format!(
                        "rule {i}: the value of tag {key:?} is longer than {MAX_TAG_LEN} bytes"
                    )));
                }
            }
        }
        Ok(rules)
    }

    /// Loads `config/asset_tags.ron` under `project_root`. A missing file is no rules; a bad file is
    /// no rules plus one warning (the cook keeps going, the registry just has no authored tags).
    pub fn load(project_root: &Path) -> (Self, Vec<String>) {
        let path = project_root.join(TAG_RULES_PATH);
        match std::fs::read_to_string(&path) {
            Ok(text) => match Self::from_ron(&text) {
                Ok(rules) => (rules, Vec::new()),
                Err(e) => (
                    Self::default(),
                    vec![format!("{}: {e}; no authored tags", path.display())],
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::default(), Vec::new()),
            Err(e) => (
                Self::default(),
                vec![format!("cannot read {}: {e}", path.display())],
            ),
        }
    }

    /// The authored tags of a registry path (a sub-asset `a.glb#mesh/X` uses its source's path).
    pub fn tags_for(&self, path: &str) -> BTreeMap<String, String> {
        let source = source_path(path);
        let mut out = BTreeMap::new();
        for rule in &self.rules {
            if rule.paths.iter().any(|glob| path_glob_match(glob, source)) {
                for (k, v) in &rule.tags {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
        out
    }
}

/// Why `key` is not a valid tag key (`None` when it is): non-empty, at most [`MAX_TAG_LEN`]
/// bytes, no `=`, no whitespace or control characters.
pub fn tag_key_problem(key: &str) -> Option<&'static str> {
    if key.is_empty() {
        Some("is empty")
    } else if key.len() > MAX_TAG_LEN {
        Some("is too long")
    } else if key.contains('=') {
        Some("contains `=`")
    } else if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Some("contains whitespace or a control character")
    } else {
        None
    }
}

/// The source part of a registry path (`assets/a.glb#mesh/X` → `assets/a.glb`).
pub fn source_path(path: &str) -> &str {
    path.split_once('#').map_or(path, |(source, _)| source)
}

/// Automatic tags of a registry path: `ext` (lower-case source extension) and `folder` (the
/// source's folder under `assets/`, omitted at the root).
pub fn automatic_tags(path: &str) -> BTreeMap<String, String> {
    let source = source_path(path);
    let mut out = BTreeMap::new();
    let (folder, name) = source.rsplit_once('/').unwrap_or(("", source));
    if let Some((_, ext)) = name
        .rsplit_once('.')
        .filter(|(stem, ext)| !stem.is_empty() && !ext.is_empty())
    {
        out.insert("ext".to_string(), ext.to_ascii_lowercase());
    }
    let folder = folder
        .strip_prefix("assets")
        .map_or(folder, |rest| rest.trim_start_matches('/'));
    if !folder.is_empty() {
        out.insert("folder".to_string(), folder.to_string());
    }
    out
}

/// Longest glob or path matched, in characters (bounded work per match).
const MAX_GLOB_CHARS: usize = 1024;

/// Path glob match with the cook's chunk semantics: `**` matches any run of characters including
/// `/` (and `**/` also matches no folder at all), `*` any run without `/`, `?` one character other
/// than `/`; `dir/**` also matches `dir` itself. A glob or path longer than 1024 characters does
/// not match, except when the glob is exactly that path plus `/**` (the shortcut above this
/// check). An identical pair longer than 1024 characters does not match. Linear-memory dynamic
/// programming, no backtracking.
pub fn path_glob_match(glob: &str, path: &str) -> bool {
    if glob.strip_suffix("/**") == Some(path) {
        return true;
    }
    #[derive(Clone, Copy)]
    enum Token {
        Any,
        Star,
        One,
        Char(char),
    }
    // Reject on the 1025th character, before storing it: the same answer as collecting and then
    // refusing, without the heap tables. The `/**` shortcut above still matches a long path.
    let mut chars = ['\0'; MAX_GLOB_CHARS];
    let mut n_chars = 0usize;
    for c in glob.chars() {
        if n_chars == MAX_GLOB_CHARS {
            return false;
        }
        chars[n_chars] = c;
        n_chars += 1;
    }
    let mut text = ['\0'; MAX_GLOB_CHARS];
    let mut n_text = 0usize;
    for c in path.chars() {
        if n_text == MAX_GLOB_CHARS {
            return false;
        }
        text[n_text] = c;
        n_text += 1;
    }
    let mut tokens = [Token::Any; MAX_GLOB_CHARS];
    let mut n_tokens = 0usize;
    let mut i = 0usize;
    while i < n_chars {
        let token = match chars[i] {
            '*' if i + 1 < n_chars && chars[i + 1] == '*' => {
                i += 2;
                if i < n_chars && chars[i] == '/' {
                    i += 1;
                }
                Token::Any
            }
            '*' => {
                i += 1;
                Token::Star
            }
            '?' => {
                i += 1;
                Token::One
            }
            c => {
                i += 1;
                Token::Char(c)
            }
        };
        tokens[n_tokens] = token;
        n_tokens += 1;
    }
    // reach[j]: the tokens so far can match text[..j].
    let mut reach = [false; MAX_GLOB_CHARS + 1];
    let mut next = [false; MAX_GLOB_CHARS + 1];
    reach[0] = true;
    for token in tokens.iter().take(n_tokens).copied() {
        next[..=n_text].fill(false);
        match token {
            Token::Any | Token::Star => {
                let mut open = false;
                for (j, (&reached, slot)) in reach[..=n_text]
                    .iter()
                    .zip(next[..=n_text].iter_mut())
                    .enumerate()
                {
                    open |= reached;
                    *slot = open;
                    if matches!(token, Token::Star) && j < n_text && text[j] == '/' {
                        open = false;
                    }
                }
            }
            Token::One => {
                for (j, &ch) in text[..n_text].iter().enumerate() {
                    next[j + 1] = reach[j] && ch != '/';
                }
            }
            Token::Char(c) => {
                for (j, &ch) in text[..n_text].iter().enumerate() {
                    next[j + 1] = reach[j] && ch == c;
                }
            }
        }
        reach[..=n_text].copy_from_slice(&next[..=n_text]);
    }
    reach[n_text]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_semantics() {
        assert!(path_glob_match("assets/props/**", "assets/props/a/b.glb"));
        assert!(path_glob_match("assets/props/**", "assets/props"));
        assert!(path_glob_match("assets/**/rock.png", "assets/rock.png"));
        assert!(path_glob_match("assets/**/rock.png", "assets/a/b/rock.png"));
        assert!(path_glob_match("assets/*.png", "assets/a.png"));
        assert!(!path_glob_match("assets/*.png", "assets/x/a.png"));
        assert!(path_glob_match("assets/?.png", "assets/a.png"));
        assert!(!path_glob_match("assets/?.png", "assets/ab.png"));
        assert!(!path_glob_match("assets/props/**", "assets/propsx/a.glb"));
        let long = "a".repeat(2000);
        assert!(path_glob_match(&format!("{long}/**"), &long));
        assert!(!path_glob_match(&long, &long));
    }

    #[test]
    fn automatic_tags_use_the_source_part() {
        let tags = automatic_tags("assets/props/Forest/Tree.GLB#mesh/Trunk");
        assert_eq!(tags.get("ext").map(String::as_str), Some("glb"));
        assert_eq!(tags.get("folder").map(String::as_str), Some("props/Forest"));
        let root = automatic_tags("assets/logo.png");
        assert_eq!(root.get("folder"), None);
        assert!(!automatic_tags("assets/.hidden").contains_key("ext"));
    }

    #[test]
    fn later_rules_override_key_by_key() {
        let rules = TagRules::from_ron(
            r#"(format_version: 1, rules: [
                (paths: ["assets/props/**"], tags: { "biome": "any", "kind": "prop" }),
                (paths: ["assets/props/forest/**"], tags: { "biome": "forest" }),
            ])"#,
        )
        .expect("rules");
        let tags = rules.tags_for("assets/props/forest/tree.glb#mesh/0");
        assert_eq!(tags.get("biome").map(String::as_str), Some("forest"));
        assert_eq!(tags.get("kind").map(String::as_str), Some("prop"));
        assert!(rules.tags_for("assets/ui/x.png").is_empty());
    }

    #[test]
    fn bad_rules_are_errors() {
        for text in [
            "(format_version: 2, rules: [])",
            "(format_version: 1, rules: [(paths: [], tags: {})])",
            r#"(format_version: 1, rules: [(paths: ["a"], tags: { "a=b": "c" })])"#,
            r#"(format_version: 1, rules: [(paths: ["a"], tags: { "a b": "c" })])"#,
            "(format_version: 1, extra: 3)",
        ] {
            assert!(TagRules::from_ron(text).is_err(), "{text}");
        }
    }
}
