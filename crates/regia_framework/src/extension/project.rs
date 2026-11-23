// SPDX-License-Identifier: MPL-2.0
//! The project's `[extensions]` table in `Regia.toml` (`specs/57` § 2.6, § 2.7).
//!
//! ```toml
//! [extensions]
//! enabled  = ["dialogue", "cheats"]
//! disabled = ["telemetry_extras"]
//! extension_paths = ["../shared/extensions"]
//!
//! [extensions.platform.android]
//! disabled = ["dev_console"]
//!
//! [extensions.grants]
//! dialogue = ["agent_method", "content_mount"]
//!
//! [extensions.settings.dialogue]
//! max_line_chars = 240
//!
//! [extensions.order.dialogue]
//! after = ["cheats"]
//! ```
//!
//! The table is optional and unversioned: `Regia.toml` ignores unknown keys and has no
//! `format_version` (`specs/21` § 2), so adding it needs no migration. [`set_enabled`] and
//! [`set_grants`] edit the file text in place, touching only the lines they change, so comments
//! and the rest of the file survive.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A per-platform delta, applied after the base lists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlatformExtensions {
    /// Extra roots on this platform.
    pub enabled: Vec<String>,
    /// Extra disabled ids on this platform.
    pub disabled: Vec<String>,
}

/// One `[extensions.grants]` entry: a capability list, or a table with `capabilities`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GrantEntry {
    /// `id = ["agent_method"]`.
    List(Vec<String>),
    /// `[extensions.grants.id]` with `capabilities = [...]`.
    Table {
        /// Granted capabilities.
        capabilities: Vec<String>,
    },
}

impl GrantEntry {
    /// The granted capabilities.
    #[must_use]
    pub fn capabilities(&self) -> &[String] {
        match self {
            Self::List(list) | Self::Table { capabilities: list } => list,
        }
    }
}

/// One `[extensions.order.<id>]` entry: where `id`'s systems run relative to other extensions in
/// a schedule they share, for extensions with no dependency edge (spec § 7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OrderEntry {
    /// Ids whose systems run first.
    pub after: Vec<String>,
    /// Ids whose systems run later.
    pub before: Vec<String>,
}

/// The `[extensions]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtensionsConfig {
    /// Root extensions.
    pub enabled: Vec<String>,
    /// Ids that never load; beats `auto_enable` and dependencies.
    pub disabled: Vec<String>,
    /// Extra discovery roots, relative to the project directory.
    pub extension_paths: Vec<String>,
    /// `[extensions.platform.<os>]` deltas.
    pub platform: BTreeMap<String, PlatformExtensions>,
    /// `[extensions.grants]`: capabilities granted per extension id.
    pub grants: BTreeMap<String, GrantEntry>,
    /// `[extensions.settings.<id>]`: opaque tables handed to the extension.
    pub settings: BTreeMap<String, toml::Table>,
    /// `[extensions.order.<id>]`: system order between extensions without a dependency edge.
    pub order: BTreeMap<String, OrderEntry>,
}

/// Why `Regia.toml` could not be read or edited.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("manifest_invalid: {0}")]
pub struct ProjectConfigError(pub String);

impl ExtensionsConfig {
    /// Reads the `[extensions]` table of `Regia.toml` text; an absent table is the default.
    ///
    /// # Errors
    ///
    /// The text is not TOML, or the table has the wrong shape.
    pub fn from_project_toml(text: &str) -> Result<Self, ProjectConfigError> {
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| ProjectConfigError(e.message().to_owned()))?;
        match table.get("extensions") {
            None => Ok(Self::default()),
            Some(value) => value
                .clone()
                .try_into()
                .map_err(|e: toml::de::Error| ProjectConfigError(format!("[extensions]: {}", e.message()))),
        }
    }

    /// This table as `Regia.toml` text (`regia plugin sync` embeds it in the generated list, so a
    /// packaged game with no `Regia.toml` still knows its grants and settings).
    ///
    /// # Errors
    ///
    /// The table cannot be written as TOML.
    pub fn to_project_toml(&self) -> Result<String, ProjectConfigError> {
        let value = toml::Value::try_from(self).map_err(|e| ProjectConfigError(e.to_string()))?;
        let mut table = toml::Table::new();
        table.insert("extensions".to_owned(), value);
        toml::to_string(&table).map_err(|e| ProjectConfigError(e.to_string()))
    }

    /// The enabled and disabled lists for `platform`, base first, then the platform delta,
    /// deduplicated in first-seen order.
    #[must_use]
    pub fn effective_lists(&self, platform: &str) -> (Vec<String>, Vec<String>) {
        let delta = self.platform.get(platform);
        let mut enabled = Vec::new();
        for id in self
            .enabled
            .iter()
            .chain(delta.into_iter().flat_map(|d| d.enabled.iter()))
        {
            if !enabled.contains(id) {
                enabled.push(id.clone());
            }
        }
        let mut disabled = Vec::new();
        for id in self
            .disabled
            .iter()
            .chain(delta.into_iter().flat_map(|d| d.disabled.iter()))
        {
            if !disabled.contains(id) {
                disabled.push(id.clone());
            }
        }
        (enabled, disabled)
    }

    /// The grants as the resolver reads them.
    #[must_use]
    pub fn grant_map(&self) -> BTreeMap<String, Vec<String>> {
        self.grants
            .iter()
            .map(|(id, g)| (id.clone(), g.capabilities().to_vec()))
            .collect()
    }
}

fn toml_list(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|s| toml::Value::String(s.clone()).to_string())
        .collect();
    format!("[{}]", quoted.join(", "))
}

fn header_of(line: &str) -> Option<&str> {
    let t = line.trim();
    if t.starts_with("[[") {
        return Some("");
    }
    let inner = t.strip_prefix('[')?;
    let end = inner.find(']')?;
    Some(inner[..end].trim())
}

fn key_of(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let eq = t.find('=')?;
    let key = t[..eq].trim();
    (!key.is_empty() && !key.starts_with('#')).then_some(key)
}

/// Lines from `start` that the value of the key on line `start` spans (brackets balanced
/// outside strings).
fn value_span(lines: &[&str], start: usize) -> usize {
    let mut depth: i32 = 0;
    let mut in_str: Option<char> = None;
    for (n, line) in lines.iter().enumerate().skip(start) {
        let text = if n == start {
            line.split_once('=').map_or("", |(_, v)| v)
        } else {
            line
        };
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            match (in_str, c) {
                (Some(_), '\\') => {
                    chars.next();
                }
                (Some(q), c) if c == q => in_str = None,
                (Some(_), _) => {}
                (None, '"' | '\'') => in_str = Some(c),
                (None, '#') => break,
                (None, '[' | '{') => depth += 1,
                (None, ']' | '}') => depth -= 1,
                _ => {}
            }
        }
        if depth <= 0 && in_str.is_none() {
            return n - start + 1;
        }
    }
    lines.len() - start
}

/// Sets `key = value` inside table `table` of TOML `text`, keeping every other line. Adds the
/// key (or the table at the end of the file) when absent.
fn set_key(text: &str, table: &str, key: &str, value: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut current = String::new();
    let mut table_header: Option<usize> = None;
    let mut table_end: Option<usize> = None;
    let mut found: Option<(usize, usize)> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(h) = header_of(line) {
            if table_header.is_some() && table_end.is_none() {
                table_end = Some(i);
            }
            current = h.to_owned();
            if current == table {
                table_header = Some(i);
                table_end = None;
            }
            continue;
        }
        if current == table && found.is_none() && key_of(line) == Some(key) {
            found = Some((i, value_span(&lines, i)));
        }
    }
    let new_line = format!("{key} = {value}");
    let mut out: Vec<String> = lines.iter().map(|s| (*s).to_owned()).collect();
    if let Some((at, span)) = found {
        out.splice(at..at + span, [new_line]);
    } else if let Some(h) = table_header {
        // After the last non-blank line of the table.
        let end = table_end.unwrap_or(out.len());
        let mut at = end;
        while at > h + 1 && out[at - 1].trim().is_empty() {
            at -= 1;
        }
        out.insert(at, new_line);
    } else {
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push(format!("[{table}]"));
        out.push(new_line);
    }
    let mut joined = out.join("\n");
    joined.push('\n');
    joined
}

/// The table path holding the enable lists for `platform` (`None` = the base lists).
fn lists_table(platform: Option<&str>) -> String {
    platform.map_or_else(|| "extensions".to_owned(), |p| format!("extensions.platform.{p}"))
}

/// Enables (`enable = true`) or disables `id` in `Regia.toml` text: adds it to `enabled` and
/// removes it from `disabled`, or the reverse, in the base lists or in
/// `[extensions.platform.<platform>]`. Returns the new text.
///
/// # Errors
///
/// The text is not TOML, or the edit did not produce the expected table (an `[extensions]`
/// written in a form the in-place editor does not handle, such as an inline table): edit by hand.
pub fn set_enabled(
    text: &str,
    id: &str,
    enable: bool,
    platform: Option<&str>,
) -> Result<String, ProjectConfigError> {
    let config = ExtensionsConfig::from_project_toml(text)?;
    let (mut enabled, mut disabled) = match platform {
        None => (config.enabled.clone(), config.disabled.clone()),
        Some(p) => config
            .platform
            .get(p)
            .map(|d| (d.enabled.clone(), d.disabled.clone()))
            .unwrap_or_default(),
    };
    if enable {
        disabled.retain(|x| x != id);
        if !enabled.iter().any(|x| x == id) {
            enabled.push(id.to_owned());
        }
    } else {
        enabled.retain(|x| x != id);
        if !disabled.iter().any(|x| x == id) {
            disabled.push(id.to_owned());
        }
    }
    let table = lists_table(platform);
    let text = set_key(text, &table, "enabled", &toml_list(&enabled));
    let text = set_key(&text, &table, "disabled", &toml_list(&disabled));
    let check = ExtensionsConfig::from_project_toml(&text)?;
    let got = match platform {
        None => (check.enabled, check.disabled),
        Some(p) => check
            .platform
            .get(p)
            .map(|d| (d.enabled.clone(), d.disabled.clone()))
            .unwrap_or_default(),
    };
    if got != (enabled, disabled) {
        return Err(ProjectConfigError(
            "could not edit [extensions] in place; edit Regia.toml by hand".into(),
        ));
    }
    Ok(text)
}

/// Adds `capabilities` to `[extensions.grants] <id>` in `Regia.toml` text (sorted, no
/// duplicates). Returns the new text.
///
/// # Errors
///
/// As [`set_enabled`].
pub fn set_grants(text: &str, id: &str, capabilities: &[String]) -> Result<String, ProjectConfigError> {
    let config = ExtensionsConfig::from_project_toml(text)?;
    let mut granted: Vec<String> = config
        .grants
        .get(id)
        .map(|g| g.capabilities().to_vec())
        .unwrap_or_default();
    granted.extend(capabilities.iter().cloned());
    granted.sort();
    granted.dedup();
    let text = set_key(text, "extensions.grants", id, &toml_list(&granted));
    let check = ExtensionsConfig::from_project_toml(&text)?;
    if check.grants.get(id).map(|g| g.capabilities().to_vec()) != Some(granted) {
        return Err(ProjectConfigError(
            "could not edit [extensions.grants] in place; edit Regia.toml by hand".into(),
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"# My game
[project]
name = "demo" # keep me

[extensions]
enabled  = ["dialogue", "cheats"]            # roots
disabled = ["telemetry_extras"]
extension_paths = ["../shared/extensions"]

[extensions.platform.android]
disabled = ["dev_console"]

[extensions.settings.dialogue]
max_line_chars = 240
"#;

    #[test]
    fn reads_the_spec_table() {
        let c = ExtensionsConfig::from_project_toml(SAMPLE).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.enabled, ["dialogue", "cheats"]);
        assert_eq!(c.extension_paths, ["../shared/extensions"]);
        let (en, dis) = c.effective_lists("android");
        assert_eq!(en, ["dialogue", "cheats"]);
        assert_eq!(dis, ["telemetry_extras", "dev_console"]);
        let (_, dis) = c.effective_lists("linux");
        assert_eq!(dis, ["telemetry_extras"]);
        assert_eq!(c.settings["dialogue"]["max_line_chars"].as_integer(), Some(240));
        assert_eq!(
            ExtensionsConfig::from_project_toml("[project]\nname='x'\n"),
            Ok(ExtensionsConfig::default())
        );
    }

    #[test]
    fn edits_keep_comments_and_other_lines() {
        let out = set_enabled(SAMPLE, "telemetry_extras", true, None).unwrap_or_else(|e| panic!("{e}"));
        assert!(out.contains("# My game"));
        assert!(out.contains("name = \"demo\" # keep me"));
        assert!(out.contains("max_line_chars = 240"));
        let c = ExtensionsConfig::from_project_toml(&out).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.enabled, ["dialogue", "cheats", "telemetry_extras"]);
        assert!(c.disabled.is_empty());
        let out = set_enabled(&out, "cheats", false, Some("android")).unwrap_or_else(|e| panic!("{e}"));
        let c = ExtensionsConfig::from_project_toml(&out).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.platform["android"].disabled, ["dev_console", "cheats"]);
        let out = set_grants(&out, "dialogue", &["content_mount".into(), "agent_method".into()])
            .unwrap_or_else(|e| panic!("{e}"));
        let c = ExtensionsConfig::from_project_toml(&out).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.grant_map()["dialogue"], ["agent_method", "content_mount"]);
    }

    #[test]
    fn adds_the_table_when_absent_and_handles_multiline_arrays() {
        let out = set_enabled("[project]\nname = \"x\"\n", "dialogue", true, None)
            .unwrap_or_else(|e| panic!("{e}"));
        let c = ExtensionsConfig::from_project_toml(&out).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.enabled, ["dialogue"]);
        let multi = "[extensions]\nenabled = [\n  \"a_one\",\n  \"b_two\", # c\n]\n\n[other]\nk = 1\n";
        let out = set_enabled(multi, "a_one", false, None).unwrap_or_else(|e| panic!("{e}"));
        let c = ExtensionsConfig::from_project_toml(&out).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.enabled, ["b_two"]);
        assert_eq!(c.disabled, ["a_one"]);
        assert!(out.contains("[other]\nk = 1"));
        // An inline table cannot be edited in place: refused, not corrupted.
        let inline = "extensions = { enabled = [\"a_one\"] }\n";
        assert!(set_enabled(inline, "b_two", true, None).is_err());
    }
}
