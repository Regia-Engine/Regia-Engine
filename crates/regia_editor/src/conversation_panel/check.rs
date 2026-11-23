// SPDX-License-Identifier: MPL-2.0
//! Validation in place: the project around an open conversation (speakers, settings, locales,
//! voice clips) and the `conv_*` validator's findings mapped onto graph nodes (ADR-3298).
//!
//! The findings are the framework validator's (`regia_framework::conversation::validate`), run on
//! the open document only, plus one editor check the runtime does not need: `conv_empty_choice`
//! (a `Choice` with no options ends the run silently, which is almost always a mistake while
//! writing).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_framework::conversation::asset::{
    CONVERSATIONS_DIR, SETTINGS_FILE, SPEAKERS_FILE, parse_settings, parse_speakers,
};
use regia_framework::conversation::{
    CLIP_EXTENSIONS, ConversationAsset, ConversationSettings, Issue, Severity, SpeakerSet, Step, ValidateEnv,
    ValidateInput, validate,
};
use regia_ui::loc_config::{LOCALIZATION_CONFIG_PATH, LocalizationConfig};
use regia_ui::localization::Localization;

use super::graph::ConvGraph;

/// The project files a conversation is checked against.
#[derive(Default)]
pub struct ProjectContext {
    /// The project root (the directory holding `assets/`), if found.
    pub root: Option<PathBuf>,
    /// `speakers.ron`.
    pub speakers: SpeakerSet,
    /// `conversation.ron`.
    pub settings: ConversationSettings,
    /// Clip paths relative to `assets/` and their length in microseconds.
    pub clips: BTreeMap<String, Option<u64>>,
    /// The project's localization, if it has `.ftl` files.
    pub loc: Option<Localization>,
    /// The locales checked (the loaded ones, or the source locale alone).
    pub locales: Vec<String>,
    /// The source locale.
    pub source: String,
    /// Problems reading the project files (shown in the panel; checking goes on with defaults).
    pub warnings: Vec<String>,
}

impl std::fmt::Debug for ProjectContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectContext")
            .field("root", &self.root)
            .field("locales", &self.locales)
            .field("clips", &self.clips.len())
            .finish()
    }
}

/// The project root for `file`: `explicit` when given, else the directory above the `assets`
/// directory that holds the file.
#[must_use]
pub fn find_root(file: &Path, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(root) = explicit {
        return Some(root.to_path_buf());
    }
    file.ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "assets"))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

fn scan_clips(dir: &Path, assets: &Path, out: &mut BTreeMap<String, Option<u64>>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            scan_clips(&path, assets, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| CLIP_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
            && let Ok(rel) = path.strip_prefix(assets)
        {
            let key = rel.to_string_lossy().replace('\\', "/");
            out.insert(key, regia_framework::conversation::clip_duration_us(&path));
        }
    }
}

impl ProjectContext {
    /// Loads the project around `root`. Missing files give defaults; unreadable ones add a
    /// warning.
    #[must_use]
    pub fn load(root: Option<PathBuf>) -> Self {
        let mut ctx = Self {
            root: root.clone(),
            source: "en".into(),
            ..Self::default()
        };
        let Some(root) = root else {
            ctx.locales = vec![ctx.source.clone()];
            return ctx;
        };
        let assets = root.join("assets");
        let conv_dir = assets.join(CONVERSATIONS_DIR);
        let read = |path: &Path| std::fs::read_to_string(path).ok();
        if let Some(text) = read(&conv_dir.join(SPEAKERS_FILE)) {
            match parse_speakers(SPEAKERS_FILE, &text) {
                Ok(s) => ctx.speakers = s,
                Err(e) => ctx.warnings.push(e.to_string()),
            }
        }
        if let Some(text) = read(&conv_dir.join(SETTINGS_FILE)) {
            match parse_settings(SETTINGS_FILE, &text) {
                Ok(s) => ctx.settings = s,
                Err(e) => ctx.warnings.push(e.to_string()),
            }
        }
        scan_clips(&assets, &assets, &mut ctx.clips);
        // Localization: the config's source locale and every locale directory's `.ftl` files.
        let mut config = LocalizationConfig::default();
        if let Some(text) = read(&root.join(LOCALIZATION_CONFIG_PATH)) {
            match LocalizationConfig::from_ron(&text) {
                Ok(c) => config = c,
                Err(e) => ctx.warnings.push(format!("{LOCALIZATION_CONFIG_PATH}: {e}")),
            }
        }
        ctx.source.clone_from(&config.source);
        let loc_dir = assets.join("localization");
        let mut sources = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&loc_dir) {
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();
            for dir in dirs {
                let tag = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if tag.starts_with('.') || (!config.locales.is_empty() && !config.locales.contains(&tag)) {
                    continue;
                }
                let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                    .map(|e| e.flatten().map(|e| e.path()).collect())
                    .unwrap_or_default();
                files.retain(|p| p.extension().is_some_and(|e| e == "ftl"));
                files.sort();
                let text: String = files
                    .iter()
                    .filter_map(|p| read(p))
                    .map(|t| format!("{t}\n"))
                    .collect();
                sources.push((tag, text));
            }
        }
        if !sources.is_empty() {
            match Localization::new(sources, Some(config.source.clone())) {
                Ok(mut loc) => {
                    loc.set_fallbacks(config.fallbacks.clone());
                    ctx.locales.clone_from(&loc.available);
                    ctx.loc = Some(loc);
                }
                Err(e) => ctx.warnings.push(format!("localization: {e}")),
            }
        }
        if ctx.locales.is_empty() {
            ctx.locales = vec![ctx.source.clone()];
        }
        ctx
    }

    /// The text of a key in the source locale, if the project defines it.
    #[must_use]
    pub fn text(&self, key: &str) -> Option<String> {
        self.text_in(&self.source, key)
    }

    /// The text of a key in `locale` (falling back along its chain).
    #[must_use]
    pub fn text_in(&self, locale: &str, key: &str) -> Option<String> {
        ValidateEnv::text(self, locale, key)
    }

    /// The clip path under `assets/` for a line in `locale`, when one exists.
    #[must_use]
    pub fn clip_for(&self, asset: &ConversationAsset, line: &str, locale: &str) -> Option<String> {
        let index = asset.line_index(line)?;
        let l = asset.line(line)?;
        let speaker = self
            .speakers
            .speakers
            .get(&l.speaker)
            .cloned()
            .unwrap_or_default();
        let chain = ValidateEnv::chain(self, locale);
        let exists = |p: &str| self.clips.contains_key(p);
        let ctx = regia_framework::conversation::VoiceContext {
            conversation: &asset.id,
            index,
            speaker_voiced: speaker.voiced,
            template: Some(self.settings.voice_path.as_str()),
            fallback_policy: self.settings.voice_fallback,
            chain: &chain,
            exists: &exists,
        };
        regia_framework::conversation::resolve_voice(l, &ctx, locale).clip
    }
}

impl ValidateEnv for ProjectContext {
    fn has_text_in(&self, locale: &str, key: &str) -> bool {
        self.loc.as_ref().is_some_and(|l| l.has_in(locale, key))
    }
    fn text(&self, locale: &str, key: &str) -> Option<String> {
        let loc = self.loc.as_ref()?;
        self.chain(locale)
            .iter()
            .find_map(|l| loc.resolve_in(l, key, &[]))
    }
    fn chain(&self, locale: &str) -> Vec<String> {
        match &self.loc {
            Some(loc) => loc.chain(locale),
            None => vec![locale.to_string()],
        }
    }
    fn clip_exists(&self, path: &str) -> bool {
        self.clips.contains_key(path)
    }
    fn clip_duration_us(&self, path: &str) -> Option<u64> {
        self.clips.get(path).copied().flatten()
    }
}

/// A finding with the node it is drawn on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIssue {
    /// The finding.
    pub issue: Issue,
    /// The node index (`None` for project-level findings).
    pub node: Option<usize>,
}

/// The node a finding belongs to.
fn node_for(issue: &Issue, graph: &ConvGraph) -> Option<usize> {
    let what = issue.line.as_str();
    if let Some(step) = what.strip_prefix("step ").and_then(|s| s.parse::<usize>().ok()) {
        return graph.node_of_step(step);
    }
    match issue.code {
        "conv_bad_label" => {
            // A missing jump target: the node that jumps; a duplicate label: its second copy;
            // a missing line: the node that names it.
            graph
                .edges
                .iter()
                .find(|e| e.to.is_none() && e.label == what)
                .map(|e| e.from)
                .or_else(|| graph.nodes.iter().rposition(|n| n.label.as_deref() == Some(what)))
                .or_else(|| graph.node_of_line(what))
        }
        _ => graph.node_of_line(what).or_else(|| graph.node_of_label(what)),
    }
}

/// Runs the validator on `asset` (every locale of the project) plus the editor's own checks, and
/// maps each finding to a node.
#[must_use]
pub fn check(asset: &ConversationAsset, graph: &ConvGraph, ctx: &ProjectContext) -> Vec<NodeIssue> {
    let _span = tracing::debug_span!(target: "regia_editor", "conversation_check").entered();
    let report = validate(
        &ValidateInput {
            conversations: std::slice::from_ref(asset),
            speakers: &ctx.speakers,
            settings: &ctx.settings,
            locales: &ctx.locales,
            source_locale: &ctx.source,
            strict: false,
        },
        ctx,
    );
    let mut issues: Vec<Issue> = report.issues;
    for (i, step) in graph.steps.iter().enumerate() {
        if let Step::Choice { options, .. } = step
            && options.is_empty()
        {
            issues.push(Issue {
                code: "conv_empty_choice",
                severity: Severity::Error,
                conversation: asset.id.clone(),
                line: format!("step {i}"),
                locale: String::new(),
                message: "the choice has no options: the run ends here".into(),
                fix: "add an option or delete the choice".into(),
            });
        }
    }
    issues
        .into_iter()
        .map(|issue| {
            let node = if issue.conversation.is_empty() {
                None
            } else {
                node_for(&issue, graph)
            };
            NodeIssue { issue, node }
        })
        .collect()
}
