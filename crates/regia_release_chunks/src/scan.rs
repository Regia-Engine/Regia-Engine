// SPDX-License-Identifier: MPL-2.0
//! Walk a tree into hard-link units and assign each unit to a group.
//!
//! Release layout (no rules file): a path with no slash, or whose first component is in
//! `core_dirs`, is `core`. The cooked directory's texture child is `{cooked}-{texture}`.
//! A `split_roots` entry becomes `{root}-{child}`. Anything else is `dir-{first}`.
//! Rules, when set, win: the first matching glob, otherwise `default`.

use std::collections::BTreeMap;
use std::fs::Metadata;
use std::path::{Component, Path, PathBuf};

use walkdir::WalkDir;

use crate::ChunkError;

/// Caps, directory roles and grouping rules for one release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSettings {
    /// Compressed size cap. Must be greater than 0 and below [`crate::TWO_GIB`].
    pub max_chunk_bytes: u64,
    /// Raw-byte target for one pack bucket. One file may be larger; the compressed cap still applies.
    pub bucket_raw: u64,
    /// First path components that join `core`, along with paths that contain no slash.
    pub core_dirs: Vec<String>,
    /// Directory whose children are the cooked group, except [`Self::texture_child`].
    pub cooked_dir: String,
    /// Child of [`Self::cooked_dir`] that forms the `{cooked}-{texture}` group.
    pub texture_child: String,
    /// Roots grouped as `{root}-{child}`.
    pub split_roots: Vec<String>,
    /// When set, these rules replace the release layout.
    pub rules: Option<GroupRules>,
    /// When true, a file with any execute bit is stored as mode 0755. Otherwise files are 0644.
    pub preserve_exec: bool,
    /// Groups a later tier may replace. Any other group that differs is `tier_chunk_differs`.
    pub tier_groups: Vec<String>,
}

impl Default for ChunkSettings {
    fn default() -> Self {
        Self {
            max_chunk_bytes: crate::DEFAULT_MAX_CHUNK_BYTES,
            bucket_raw: crate::DEFAULT_BUCKET_RAW_BYTES,
            core_dirs: vec!["config".to_string(), "spec".to_string()],
            cooked_dir: "cooked".to_string(),
            texture_child: "tex".to_string(),
            split_roots: Vec::new(),
            rules: None,
            preserve_exec: false,
            tier_groups: vec!["cooked-tex".to_string()],
        }
    }
}

impl ChunkSettings {
    /// Rejects a cap of 0 or a cap at or above 2 GiB (`chunk_cap_invalid`).
    ///
    /// # Errors
    ///
    /// `chunk_cap_invalid` when [`Self::max_chunk_bytes`] is outside that range.
    pub fn enforce_cap(&self) -> Result<(), ChunkError> {
        if self.max_chunk_bytes == 0 || self.max_chunk_bytes >= crate::TWO_GIB {
            return Err(ChunkError::new(
                "chunk_cap_invalid",
                format!(
                    "chunk cap {} must be greater than 0 and below {} bytes",
                    self.max_chunk_bytes,
                    crate::TWO_GIB
                ),
            ));
        }
        Ok(())
    }
}

/// How a walked path is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeKind {
    /// A regular file.
    File,
    /// A symbolic link. The tree walk does not follow it.
    Symlink,
}

/// One path in the tree.
#[derive(Debug, Clone)]
pub(crate) struct Node {
    /// Relative path with `/` separators.
    pub rel: String,
    /// Absolute or source-joined path to read.
    pub abs: PathBuf,
    /// Payload bytes. Zero for a symlink.
    pub size: u64,
    /// ustar mode (0644, 0755 or 0777).
    pub mode: u32,
    /// File or symlink.
    pub kind: NodeKind,
    /// Symlink target, rejected when absolute or when it contains `..`.
    pub link_target: Option<String>,
}

/// A hard-link set. `stored` is the smallest path. `links` are the other paths.
#[derive(Debug, Clone)]
pub(crate) struct Unit {
    /// The path whose bytes are stored.
    pub stored: Node,
    /// Extra hard links, sorted by path.
    pub links: Vec<Node>,
}

impl Unit {
    pub(crate) fn unpacked(&self) -> u64 {
        self.stored.size
    }

    pub(crate) fn file_names(&self) -> Vec<String> {
        let mut names = vec![self.stored.rel.clone()];
        names.extend(self.links.iter().map(|link| link.rel.clone()));
        names.sort();
        names
    }
}

/// One glob rule. Earlier rules win.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRule {
    /// Group name written into the manifest.
    pub group: String,
    /// Globs tested against the relative path.
    pub globs: Vec<String>,
}

/// Ordered grouping rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupRules {
    /// First match wins. An empty list assigns every path to `default`.
    pub rules: Vec<GroupRule>,
}

impl GroupRules {
    /// The group of `rel`.
    #[must_use]
    pub fn assign(&self, rel: &str) -> String {
        for rule in &self.rules {
            if rule.globs.iter().any(|glob| glob_match(glob, rel)) {
                return rule.group.clone();
            }
        }
        "default".to_string()
    }
}

/// Parses a rules file. `[[rule]]` tables keep their order and are tested first.
/// A top-level array of strings, or a table with `globs`, is also a rule.
///
/// # Errors
///
/// `rules_invalid` when the TOML is not a table of those shapes.
pub fn load_rules_str(text: &str) -> Result<GroupRules, ChunkError> {
    let value: toml::Value =
        toml::from_str(text).map_err(|err| ChunkError::new("rules_invalid", err.to_string()))?;
    let table = value
        .as_table()
        .ok_or_else(|| ChunkError::new("rules_invalid", "rules must be a TOML table"))?;
    let mut rules = Vec::new();
    if let Some(list) = table.get("rule").and_then(toml::Value::as_array) {
        for item in list {
            rules.push(rule_from_table(item)?);
        }
    }
    for (key, value) in table {
        if key == "rule" {
            continue;
        }
        let globs = match value {
            toml::Value::Array(_) => string_list(value)?,
            toml::Value::Table(inner) => {
                let globs = inner.get("globs").ok_or_else(|| {
                    ChunkError::new("rules_invalid", format!("[{key}] needs a globs array"))
                })?;
                string_list(globs)?
            }
            _ => {
                return Err(ChunkError::new(
                    "rules_invalid",
                    format!("rule `{key}` must be an array or a table with globs"),
                ));
            }
        };
        rules.push(GroupRule {
            group: key.clone(),
            globs,
        });
    }
    Ok(GroupRules { rules })
}

fn rule_from_table(value: &toml::Value) -> Result<GroupRule, ChunkError> {
    let group = value
        .get("group")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| ChunkError::new("rules_invalid", "[[rule]] needs a group string"))?;
    let globs = value
        .get("globs")
        .ok_or_else(|| ChunkError::new("rules_invalid", format!("[[rule]] {group} needs globs")))?;
    Ok(GroupRule {
        group: group.to_string(),
        globs: string_list(globs)?,
    })
}

fn string_list(value: &toml::Value) -> Result<Vec<String>, ChunkError> {
    let array = value
        .as_array()
        .ok_or_else(|| ChunkError::new("rules_invalid", "globs must be an array of strings"))?;
    let mut out = Vec::with_capacity(array.len());
    for item in array {
        let text = item
            .as_str()
            .ok_or_else(|| ChunkError::new("rules_invalid", "a glob must be a string"))?;
        out.push(text.to_string());
    }
    Ok(out)
}

/// `2GiB`, `512MiB`, `64KiB` or a plain byte count.
///
/// # Errors
///
/// `chunk_cap_invalid` when the text is not a size.
pub fn parse_byte_size(text: &str) -> Result<u64, ChunkError> {
    let trimmed = text.trim();
    let (number, scale) = if let Some(rest) = trimmed.strip_suffix("GiB") {
        (rest, 1024u64 * 1024 * 1024)
    } else if let Some(rest) = trimmed.strip_suffix("MiB") {
        (rest, 1024u64 * 1024)
    } else if let Some(rest) = trimmed.strip_suffix("KiB") {
        (rest, 1024u64)
    } else {
        (trimmed, 1u64)
    };
    let count: u64 = number
        .trim()
        .parse()
        .map_err(|_| ChunkError::new("chunk_cap_invalid", format!("`{text}` is not a byte size")))?;
    count
        .checked_mul(scale)
        .ok_or_else(|| ChunkError::new("chunk_cap_invalid", format!("`{text}` overflows a byte count")))
}

pub(crate) fn collect(
    root: &Path,
    settings: &ChunkSettings,
    refuse_special: bool,
) -> Result<BTreeMap<String, Vec<Unit>>, ChunkError> {
    let mut nodes = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry =
            entry.map_err(|err| ChunkError::new("io_error", format!("walking {}: {err}", root.display())))?;
        if entry.path() == root {
            continue;
        }
        let file_type = entry.file_type();
        if file_type.is_dir() {
            continue;
        }
        let rel = rel_slash(root, entry.path())?;
        check_rel(&rel)?;
        if file_type.is_symlink() {
            let target =
                std::fs::read_link(entry.path()).map_err(|err| ChunkError::io(entry.path(), &err))?;
            let link_target = link_text(&target)?;
            nodes.push(Node {
                rel,
                abs: entry.path().to_path_buf(),
                size: 0,
                mode: 0o777,
                kind: NodeKind::Symlink,
                link_target: Some(link_target),
            });
            continue;
        }
        if !file_type.is_file() {
            if refuse_special {
                return Err(ChunkError::new(
                    "not_regular",
                    format!("{} is not a regular file", entry.path().display()),
                ));
            }
            continue;
        }
        let meta = std::fs::metadata(entry.path()).map_err(|err| ChunkError::io(entry.path(), &err))?;
        nodes.push(Node {
            rel,
            abs: entry.path().to_path_buf(),
            size: meta.len(),
            mode: file_mode(&meta, settings.preserve_exec),
            kind: NodeKind::File,
            link_target: None,
        });
    }
    nodes.sort_by(|left, right| left.rel.cmp(&right.rel));
    let units = hardlink_units(nodes)?;
    let mut groups: BTreeMap<String, Vec<Unit>> = BTreeMap::new();
    for unit in units {
        let name = group_of(&unit.stored.rel, settings);
        groups.entry(name).or_default().push(unit);
    }
    Ok(groups)
}

fn group_of(rel: &str, settings: &ChunkSettings) -> String {
    if let Some(rules) = &settings.rules {
        return rules.assign(rel);
    }
    release_group(rel, settings)
}

fn release_group(rel: &str, settings: &ChunkSettings) -> String {
    let mut parts = rel.split('/');
    let first = parts.next().unwrap_or(rel);
    let second = parts.next();
    if !rel.contains('/') || settings.core_dirs.iter().any(|dir| dir == first) {
        return "core".to_string();
    }
    if first == settings.cooked_dir {
        if second == Some(settings.texture_child.as_str()) {
            return format!("{}-{}", settings.cooked_dir, settings.texture_child);
        }
        return settings.cooked_dir.clone();
    }
    if settings.split_roots.iter().any(|root| root == first) {
        let child = second.unwrap_or("root");
        return format!("{first}-{child}");
    }
    format!("dir-{first}")
}

fn hardlink_units(nodes: Vec<Node>) -> Result<Vec<Unit>, ChunkError> {
    let mut by_inode: BTreeMap<(u64, u64), Vec<usize>> = BTreeMap::new();
    for (index, node) in nodes.iter().enumerate() {
        if node.kind != NodeKind::File {
            continue;
        }
        if let Some(key) = inode_key(&node.abs)? {
            by_inode.entry(key).or_default().push(index);
        }
    }
    let mut consumed = vec![false; nodes.len()];
    let mut units = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        if consumed[index] {
            continue;
        }
        if node.kind == NodeKind::File
            && let Some(key) = inode_key(&node.abs)?
            && let Some(members) = by_inode.get(&key)
            && members.len() > 1
        {
            let stored = nodes[members[0]].clone();
            let mut links = Vec::new();
            for member in members.iter().skip(1) {
                let mut link = nodes[*member].clone();
                link.mode = 0o777;
                links.push(link);
                consumed[*member] = true;
            }
            consumed[index] = true;
            units.push(Unit { stored, links });
            continue;
        }
        consumed[index] = true;
        units.push(Unit {
            stored: node.clone(),
            links: Vec::new(),
        });
    }
    units.sort_by(|left, right| left.stored.rel.cmp(&right.stored.rel));
    Ok(units)
}

fn rel_slash(root: &Path, path: &Path) -> Result<String, ChunkError> {
    let rel = path.strip_prefix(root).map_err(|_| {
        ChunkError::new(
            "path_rejected",
            format!("{} is outside {}", path.display(), root.display()),
        )
    })?;
    let mut text = String::new();
    for (index, component) in rel.components().enumerate() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str().ok_or_else(|| {
                    ChunkError::new("path_rejected", format!("{} is not UTF-8", path.display()))
                })?;
                if index > 0 {
                    text.push('/');
                }
                text.push_str(part);
            }
            _ => {
                return Err(ChunkError::new(
                    "path_rejected",
                    format!("{} is not a relative path of normal components", path.display()),
                ));
            }
        }
    }
    Ok(text)
}

pub(crate) fn check_rel(rel: &str) -> Result<(), ChunkError> {
    if rel.is_empty()
        || rel.starts_with('/')
        || rel.starts_with('\\')
        || rel
            .bytes()
            .any(|byte| matches!(byte, 0 | b'\n' | b'\r' | b'\t' | b'\\'))
        || rel
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(ChunkError::new(
            "path_rejected",
            format!("path `{rel}` is empty, absolute, or contains `..` or a control character"),
        ));
    }
    if rel.len() > 4096 {
        return Err(ChunkError::new(
            "path_too_long",
            format!("path `{rel}` is longer than 4096 bytes"),
        ));
    }
    Ok(())
}

fn link_text(target: &Path) -> Result<String, ChunkError> {
    if target.is_absolute() {
        return Err(ChunkError::new(
            "path_rejected",
            format!("symlink target {} is absolute", target.display()),
        ));
    }
    let mut text = String::new();
    for (index, component) in target.components().enumerate() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| ChunkError::new("path_rejected", "symlink target is not UTF-8"))?;
                if index > 0 {
                    text.push('/');
                }
                text.push_str(part);
            }
            Component::CurDir => {}
            _ => {
                return Err(ChunkError::new(
                    "path_rejected",
                    format!("symlink target {} leaves the tree", target.display()),
                ));
            }
        }
    }
    if text.is_empty() {
        return Err(ChunkError::new("path_rejected", "symlink target is empty"));
    }
    Ok(text)
}

fn file_mode(meta: &Metadata, preserve_exec: bool) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if preserve_exec && meta.permissions().mode() & 0o111 != 0 {
            return 0o755;
        }
    }
    let _ = (meta, preserve_exec);
    0o644
}

fn inode_key(path: &Path) -> Result<Option<(u64, u64)>, ChunkError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path).map_err(|err| ChunkError::io(path, &err))?;
        if meta.is_file() && meta.nlink() > 1 {
            return Ok(Some((meta.dev(), meta.ino())));
        }
        Ok(None)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

fn glob_match(pattern: &str, path: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if !pattern.contains(['*', '?']) {
        return path == pattern || path.starts_with(&format!("{pattern}/"));
    }
    if !pattern.contains('/') {
        let base = path.rsplit_once('/').map(|(_, base)| base).unwrap_or(path);
        if match_glob(pattern.as_bytes(), base.as_bytes()) {
            return true;
        }
    }
    match_glob(pattern.as_bytes(), path.as_bytes())
}

fn match_glob(pattern: &[u8], text: &[u8]) -> bool {
    fn rec(pattern: &[u8], text: &[u8]) -> bool {
        if pattern.is_empty() {
            return text.is_empty();
        }
        if pattern.starts_with(b"**") {
            let rest = if pattern.len() > 2 && pattern[2] == b'/' {
                &pattern[3..]
            } else {
                &pattern[2..]
            };
            if rest.is_empty() {
                return true;
            }
            let mut at = 0usize;
            loop {
                if rec(rest, &text[at..]) {
                    return true;
                }
                if at >= text.len() {
                    return false;
                }
                at += 1;
            }
        }
        if pattern[0] == b'*' {
            let mut at = 0usize;
            loop {
                if rec(&pattern[1..], &text[at..]) {
                    return true;
                }
                if at >= text.len() || text[at] == b'/' {
                    return false;
                }
                at += 1;
            }
        }
        if text.is_empty() {
            return false;
        }
        if pattern[0] == b'?' {
            if text[0] == b'/' {
                return false;
            }
            return rec(&pattern[1..], &text[1..]);
        }
        if pattern[0] != text[0] {
            return false;
        }
        rec(&pattern[1..], &text[1..])
    }
    rec(pattern, text)
}
