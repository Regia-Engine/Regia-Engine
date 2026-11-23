// SPDX-License-Identifier: MPL-2.0
//! The Content Browser's search query (`book/agents/editor-asset-browser.md`, ADR-1329).
//!
//! A query is a list of whitespace-separated words:
//!
//! - a plain word must appear in the project-relative path, ignoring case (`rock`, `props/cr`);
//! - `-word` hides paths that contain the word;
//! - `type:<name>` (or `t:<name>`) keeps one asset type: `level`, `prefab`, `mesh`, `texture`,
//!   `material`, `audio` or `other` (several `type:` words widen the filter);
//! - `ext:<suffix>` keeps file names ending in `.<suffix>` (`ext:png`, `ext:material.ron`);
//! - `tag:<key>` keeps assets with that tag, `tag:<key>=<value>` with that value (any case; tags
//!   come from `config/asset_tags.ron` and the automatic `ext`/`folder` tags; several `tag:` words
//!   widen the filter);
//! - `is:<state>` keeps assets in a state: `modified`, `unreferenced`, `problem` or `review`
//!   ([`AssetState`]; several `is:` words widen the filter);
//! - `meta:<key>=<value>` keeps assets whose `.meta` field `<key>` contains `<value>` (`meta:<key>`
//!   alone: the field is set); every `meta:` word must match.
//!
//! A plain word also matches the values of an asset's tags and `.meta` fields ([`AssetFacts`]).
//! Matches are ranked: a file name that starts with the first word, then a file name that holds
//! every word, then a match through the folder names only, then a match through the tags and
//! `.meta` fields; each rank is sorted by path. Without facts ([`AssetQuery::rank`]) the `tag:`,
//! `is:` and `meta:` words match nothing.
//!
//! [`AssetQuery::refines`] says when a new query can only match a subset of an older one (typing
//! one more letter, adding a word or a filter). The index then filters the older result instead of
//! every asset, so typing into the search box over 50 000 assets touches fewer entries per key.

/// The asset types a `type:` filter can name, in the order the agent guide lists them.
pub const ASSET_TYPES: [&str; 7] = ["level", "prefab", "mesh", "texture", "material", "audio", "other"];

/// The search type of a project-relative path (by its file name's suffix, any case).
#[must_use]
pub fn asset_type(path: &str) -> &'static str {
    asset_type_lower(&path.to_ascii_lowercase())
}

/// [`asset_type`] of a path that is already lowercase.
fn asset_type_lower(lower: &str) -> &'static str {
    const TEXTURE: [&str; 9] = [
        ".png", ".jpg", ".jpeg", ".tga", ".bmp", ".hdr", ".exr", ".ktx2", ".dds",
    ];
    const AUDIO: [&str; 4] = [".wav", ".ogg", ".flac", ".mp3"];
    const MATERIAL: [&str; 3] = [".material.ron", ".matgraph.ron", ".matinst.ron"];
    if lower.ends_with(".level.ron") {
        "level"
    } else if lower.ends_with(".prefab.ron") || lower.ends_with(".variant.ron") {
        "prefab"
    } else if lower.ends_with(".gltf") || lower.ends_with(".glb") {
        "mesh"
    } else if TEXTURE.iter().any(|ext| lower.ends_with(ext)) {
        "texture"
    } else if MATERIAL.iter().any(|ext| lower.ends_with(ext)) {
        "material"
    } else if AUDIO.iter().any(|ext| lower.ends_with(ext)) {
        "audio"
    } else {
        "other"
    }
}

/// An asset state a filter chip or an `is:` word names (`docs/specs/19-editor.md` § 2 content
/// browser filters, ADR-7759).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AssetState {
    /// An open document of it has unsaved edits, or its source is newer than its cooked file.
    Modified,
    /// No project file references it (the delete check's referencer rule).
    Unreferenced,
    /// The last validator run reported a finding for it.
    Problem,
    /// Its `.meta` provenance waits for a review (made by an agent or a generator, or the
    /// provenance policy asks for one).
    Review,
}

impl AssetState {
    /// Every state, in chip order.
    pub const ALL: [Self; 4] = [Self::Modified, Self::Unreferenced, Self::Problem, Self::Review];

    /// The `is:` word and protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Modified => "modified",
            Self::Unreferenced => "unreferenced",
            Self::Problem => "problem",
            Self::Review => "review",
        }
    }

    /// The chip label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Modified => "Modified",
            Self::Unreferenced => "Unreferenced",
            Self::Problem => "Has problems",
            Self::Review => "Review pending",
        }
    }

    /// The state an `is:` word or protocol name names.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.name() == name)
    }

    fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// What the index knows about one asset besides its path: its tags, its `.meta` fields and its
/// states. Keys are lowercase.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssetFacts {
    /// Tag key → value (`config/asset_tags.ron` plus the automatic `ext` and `folder` tags).
    pub tags: std::collections::BTreeMap<String, String>,
    /// `.meta` field → value: `id`, `importer`, `tool`, `origin`, `generator`, `client`, `model`,
    /// `created`, `reviewer`, and each provenance extension field.
    pub meta: std::collections::BTreeMap<String, String>,
    states: u8,
    /// Lowercase `value` lines of the authored tags and the `.meta` fields (plain words match it).
    text: String,
}

impl AssetFacts {
    /// Facts from tags and `.meta` fields (keys are lowercased).
    #[must_use]
    pub fn new(
        tags: impl IntoIterator<Item = (String, String)>,
        meta: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let tags: std::collections::BTreeMap<String, String> =
            tags.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect();
        let meta: std::collections::BTreeMap<String, String> =
            meta.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect();
        let mut text = String::new();
        for (key, value) in &tags {
            // The automatic tags repeat the path; the path match already covers them.
            if matches!(key.as_str(), "ext" | "folder") {
                continue;
            }
            text.push_str(&value.to_lowercase());
            text.push('\n');
        }
        for (key, value) in &meta {
            // An asset id is not words; `meta:id=` finds it.
            if key == "id" {
                continue;
            }
            text.push_str(&value.to_lowercase());
            text.push('\n');
        }
        Self {
            tags,
            meta,
            states: 0,
            text,
        }
    }

    /// Marks or clears a state.
    pub fn set_state(&mut self, state: AssetState, on: bool) {
        if on {
            self.states |= state.bit();
        } else {
            self.states &= !state.bit();
        }
    }

    /// Whether the asset is in `state`.
    #[must_use]
    pub fn has_state(&self, state: AssetState) -> bool {
        self.states & state.bit() != 0
    }

    /// The states the asset is in, in chip order.
    #[must_use]
    pub fn states(&self) -> Vec<AssetState> {
        AssetState::ALL
            .into_iter()
            .filter(|s| self.has_state(*s))
            .collect()
    }
}

/// A query the user typed could not be understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    /// The protocol code: `search_bad_type` (unknown `type:` name), `search_bad_state` (unknown
    /// `is:` name) or `search_empty_filter` (`type:`, `t:`, `ext:`, `tag:`, `is:`, `meta:` or `-`
    /// with nothing after it).
    pub code: &'static str,
    /// A sentence for the search box and the agent reply.
    pub message: String,
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for QueryError {}

/// A parsed search (see the module docs for the syntax).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssetQuery {
    /// Words every match contains (lowercase).
    pub terms: Vec<String>,
    /// Words no match contains (lowercase).
    pub excluded: Vec<String>,
    /// Allowed asset types (empty = any), sorted.
    pub types: Vec<&'static str>,
    /// Allowed file-name suffixes without the dot (lowercase, empty = any), sorted.
    pub exts: Vec<String>,
    /// Allowed tags (`key`, optional exact value; lowercase; empty = any), sorted.
    pub tags: Vec<(String, Option<String>)>,
    /// Allowed states (empty = any), sorted.
    pub states: Vec<AssetState>,
    /// `.meta` fields every match has (`key`, optional value it contains; lowercase), sorted.
    pub meta: Vec<(String, Option<String>)>,
}

/// `key` or `key=value` (lowercase), `None` for an empty key.
fn key_value(text: &str) -> Option<(String, Option<String>)> {
    let (key, value) = match text.split_once('=') {
        Some((key, value)) => (key, Some(value.to_string()).filter(|v| !v.is_empty())),
        None => (text, None),
    };
    (!key.is_empty()).then(|| (key.to_string(), value))
}

/// The lowercase, `/`-separated form of a project-relative path that queries match against, and
/// the byte offset of its file name.
#[must_use]
pub fn search_key(path: &str) -> (String, usize) {
    let lower = path.to_lowercase().replace('\\', "/");
    let name_at = lower.rfind('/').map_or(0, |slash| slash + 1);
    (lower, name_at)
}

impl AssetQuery {
    /// Parses the search box text. An empty or blank text is the empty query (matches all).
    ///
    /// # Errors
    ///
    /// An unknown `type:` name or a filter word with nothing after it ([`QueryError::code`]).
    pub fn parse(text: &str) -> Result<Self, QueryError> {
        let mut query = Self::default();
        for word in text.split_whitespace() {
            let lower = word.to_lowercase().replace('\\', "/");
            if let Some(name) = lower.strip_prefix("type:").or_else(|| lower.strip_prefix("t:")) {
                if name.is_empty() {
                    return Err(empty_filter(word));
                }
                let Some(known) = ASSET_TYPES.iter().find(|known| **known == name) else {
                    return Err(QueryError {
                        code: "search_bad_type",
                        message: format!("unknown asset type `{name}` (use {})", ASSET_TYPES.join(", ")),
                    });
                };
                query.types.push(*known);
            } else if let Some(ext) = lower.strip_prefix("ext:") {
                let ext = ext.trim_start_matches('.');
                if ext.is_empty() {
                    return Err(empty_filter(word));
                }
                query.exts.push(ext.to_string());
            } else if let Some(tag) = lower.strip_prefix("tag:") {
                query.tags.push(key_value(tag).ok_or_else(|| empty_filter(word))?);
            } else if let Some(field) = lower.strip_prefix("meta:") {
                query
                    .meta
                    .push(key_value(field).ok_or_else(|| empty_filter(word))?);
            } else if let Some(name) = lower.strip_prefix("is:") {
                if name.is_empty() {
                    return Err(empty_filter(word));
                }
                let Some(state) = AssetState::from_name(name) else {
                    let names: Vec<&str> = AssetState::ALL.iter().map(|s| s.name()).collect();
                    return Err(QueryError {
                        code: "search_bad_state",
                        message: format!("unknown asset state `{name}` (use {})", names.join(", ")),
                    });
                };
                query.states.push(state);
            } else if let Some(excluded) = lower.strip_prefix('-') {
                if excluded.is_empty() {
                    return Err(empty_filter(word));
                }
                query.excluded.push(excluded.to_string());
            } else {
                query.terms.push(lower);
            }
        }
        query.types.sort_unstable();
        query.types.dedup();
        query.exts.sort();
        query.exts.dedup();
        query.tags.sort();
        query.tags.dedup();
        query.states.sort_unstable();
        query.states.dedup();
        query.meta.sort();
        query.meta.dedup();
        Ok(query)
    }

    /// Whether the query keeps every asset (no words, no filters).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
            && self.excluded.is_empty()
            && self.types.is_empty()
            && self.exts.is_empty()
            && !self.uses_facts()
    }

    /// Whether the query has a `tag:`, `is:` or `meta:` word (it needs [`AssetFacts`]).
    #[must_use]
    pub fn uses_facts(&self) -> bool {
        !self.tags.is_empty() || !self.states.is_empty() || !self.meta.is_empty()
    }

    /// Whether the query has an `is:unreferenced` word (the index then reads every project
    /// file's references).
    #[must_use]
    pub fn wants(&self, state: AssetState) -> bool {
        self.states.contains(&state)
    }

    /// [`Self::rank`] with the asset's facts: `tag:`, `is:` and `meta:` words test them, and a
    /// plain word that is not in the path may match a tag or `.meta` value (rank 3).
    #[must_use]
    pub fn rank_with(&self, lower: &str, name_at: usize, facts: Option<&AssetFacts>) -> Option<u8> {
        let empty = AssetFacts::default();
        let facts = facts.unwrap_or(&empty);
        let mut through_facts = false;
        for term in &self.terms {
            if !lower.contains(term.as_str()) {
                if !facts.text.contains(term.as_str()) {
                    return None;
                }
                through_facts = true;
            }
        }
        if !self.tags.is_empty()
            && !self.tags.iter().any(|(key, value)| {
                facts
                    .tags
                    .get(key)
                    .is_some_and(|have| value.as_ref().is_none_or(|want| have.to_lowercase() == *want))
            })
        {
            return None;
        }
        if !self.states.is_empty() && !self.states.iter().any(|state| facts.has_state(*state)) {
            return None;
        }
        if !self.meta.iter().all(|(key, value)| {
            facts.meta.get(key).is_some_and(|have| {
                value
                    .as_ref()
                    .is_none_or(|want| have.to_lowercase().contains(want.as_str()))
            })
        }) {
            return None;
        }
        let rank = self.path_rank(lower, name_at, true)?;
        Some(if through_facts { 3 } else { rank })
    }

    /// The rank of a path (0 best) or `None` when it does not match. `lower` and `name_at` come
    /// from [`search_key`].
    #[must_use]
    pub fn rank(&self, lower: &str, name_at: usize) -> Option<u8> {
        if self.uses_facts() {
            return None;
        }
        self.path_rank(lower, name_at, false)
    }

    /// The path part of [`Self::rank`]; `terms_checked` skips the word test (done with facts).
    fn path_rank(&self, lower: &str, name_at: usize, terms_checked: bool) -> Option<u8> {
        if !terms_checked && !self.terms.iter().all(|term| lower.contains(term.as_str())) {
            return None;
        }
        if self.excluded.iter().any(|word| lower.contains(word.as_str())) {
            return None;
        }
        if !self.types.is_empty() && !self.types.contains(&asset_type_lower(lower)) {
            return None;
        }
        let name = lower.get(name_at..).unwrap_or(lower);
        if !self.exts.is_empty()
            && !self.exts.iter().any(|ext| {
                name.strip_suffix(ext.as_str())
                    .is_some_and(|stem| stem.ends_with('.'))
            })
        {
            return None;
        }
        let Some(first) = self.terms.first() else {
            return Some(0);
        };
        if name.starts_with(first.as_str()) {
            Some(0)
        } else if self.terms.iter().all(|term| name.contains(term.as_str())) {
            Some(1)
        } else if self.terms.iter().all(|term| lower.contains(term.as_str())) {
            Some(2)
        } else {
            // A word matched a tag or `.meta` value only (rank 3 in `rank_with`).
            Some(3)
        }
    }

    /// The rank of a project-relative path (see [`Self::rank`]).
    #[must_use]
    pub fn rank_path(&self, path: &str) -> Option<u8> {
        let (lower, name_at) = search_key(path);
        self.rank(&lower, name_at)
    }

    /// Whether every path this query matches is also matched by `older`, so a search can filter
    /// `older`'s result instead of every asset. True when each older word is part of a word of
    /// this query, each older exclusion is still excluded, and the type and suffix filters are
    /// the same or narrower.
    #[must_use]
    pub fn refines(&self, older: &Self) -> bool {
        let terms = older
            .terms
            .iter()
            .all(|old| self.terms.iter().any(|new| new.contains(old.as_str())));
        let excluded = older
            .excluded
            .iter()
            .all(|old| self.excluded.iter().any(|new| old.contains(new.as_str())));
        let types = older.types.is_empty()
            || (!self.types.is_empty() && self.types.iter().all(|t| older.types.contains(t)));
        let exts = older.exts.is_empty()
            || (!self.exts.is_empty() && self.exts.iter().all(|e| older.exts.contains(e)));
        let tags = older.tags.is_empty()
            || (!self.tags.is_empty() && self.tags.iter().all(|t| older.tags.contains(t)));
        let states = older.states.is_empty()
            || (!self.states.is_empty() && self.states.iter().all(|s| older.states.contains(s)));
        // Each older field test is still made, with the same or a longer value.
        let meta = older.meta.iter().all(|(key, old)| {
            self.meta.iter().any(|(new_key, new)| {
                new_key == key
                    && match (old, new) {
                        (None, _) => true,
                        (Some(old), Some(new)) => new.contains(old.as_str()),
                        (Some(_), None) => false,
                    }
            })
        });
        terms && excluded && types && exts && tags && states && meta
    }
}

fn empty_filter(word: &str) -> QueryError {
    QueryError {
        code: "search_empty_filter",
        message: format!("`{word}` needs a value after it"),
    }
}

/// Filters and ranks `paths` with `query`: `(rank, path)` sorted. The reference the index's
/// cached search must agree with.
#[must_use]
pub fn search_paths<'a>(query: &AssetQuery, paths: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut ranked: Vec<(u8, &str)> = paths
        .into_iter()
        .filter_map(|path| query.rank_path(path).map(|rank| (rank, path)))
        .collect();
    ranked.sort_unstable();
    ranked.into_iter().map(|(_, path)| path).collect()
}

// ---------------------------------------------------------------------------------------------
// Token completion (P-020, ADR-7766): Tab in the universal search and the Content Browser search.

/// Filter keys the Content Browser's query reads (completed after a word with no `:`).
pub const CONTENT_FILTER_KEYS: [&str; 3] = ["type:", "t:", "ext:"];

/// What [`complete_token`] may complete to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionVocab {
    /// Whole tokens offered for a word with no `:` (`a:`, `type:`, `kind:`...).
    pub keys: Vec<String>,
    /// Values after `type:` / `t:` (asset types, then component names).
    pub types: Vec<String>,
    /// Values after `kind:` (`none`, `?` and the kinds the level knows).
    pub kinds: Vec<String>,
    /// Values after `tag:` (the tags the level's entities hold).
    pub tags: Vec<String>,
}

impl CompletionVocab {
    /// The Content Browser search box's vocabulary: `type:`, `t:` and `ext:`, and the asset types.
    #[must_use]
    pub fn content_browser() -> Self {
        Self {
            keys: CONTENT_FILTER_KEYS.iter().map(|k| (*k).to_owned()).collect(),
            types: ASSET_TYPES.iter().map(|t| (*t).to_owned()).collect(),
            kinds: Vec::new(),
            tags: Vec::new(),
        }
    }

    /// The universal search's vocabulary in `world`: provider prefixes and every filter key; asset
    /// types; `none`, `?` and the registered kinds; the tags held by the level's entities (read
    /// from at most 20 000 entities).
    #[must_use]
    pub fn of_world(world: &regia_ecs::prelude::World) -> Self {
        let mut keys: Vec<String> = crate::universal_search::Provider::ALL
            .iter()
            .map(|p| p.prefix().to_owned())
            .collect();
        keys.extend(["type:", "t:", "ext:", "kind:", "tag:"].map(str::to_owned));
        let mut kinds = vec!["none".to_owned(), "?".to_owned()];
        if let Some(registry) = world.get_resource::<regia_scene::kind::KindRegistry>() {
            kinds.extend(registry.kinds().iter().map(|k| k.name.to_lowercase()));
        }
        let mut tags = std::collections::BTreeSet::new();
        for entity in world.iter_entities().take(20_000) {
            if let Some(held) = entity.get::<regia_scene::components::Tags>() {
                tags.extend(held.0.iter().map(|t| t.as_str().to_lowercase()));
            }
        }
        Self {
            keys,
            types: ASSET_TYPES.iter().map(|t| (*t).to_owned()).collect(),
            kinds,
            tags: tags.into_iter().collect(),
        }
    }
}

/// Completes the last token of `text` (the word after the last space; nothing when the text ends
/// in a space). A word with no `:` completes to a filter key or provider prefix; after `type:`,
/// `t:`, `kind:` or `tag:` the value completes from `vocab`. With one match the token becomes it;
/// with several, the token grows to their longest common start, and when it cannot grow it becomes
/// the first match in order. `None` when nothing matches or the token is already complete.
#[must_use]
pub fn complete_token(text: &str, vocab: &CompletionVocab) -> Option<String> {
    let start = text
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let token = &text[start..];
    if token.is_empty() {
        return None;
    }
    // A provider prefix may lead a filter (`e:kind:li`): complete the part after it.
    let lead = crate::universal_search::Provider::ALL
        .iter()
        .map(|p| p.prefix())
        .find(|p| {
            token.len() > p.len() && token.to_lowercase().starts_with(p) && token[p.len()..].contains(':')
        })
        .map_or(0, str::len);
    let body = &token[lead..];
    let (key, value, pool): (&str, &str, &[String]) = match body.find(':') {
        Some(colon) => {
            let key = &body[..=colon];
            let pool: &[String] = match key.to_lowercase().as_str() {
                "type:" | "t:" => &vocab.types,
                "kind:" => &vocab.kinds,
                "tag:" => &vocab.tags,
                _ => return None,
            };
            (key, &body[colon + 1..], pool)
        }
        None => ("", body, &vocab.keys),
    };
    let lower = value.to_lowercase();
    let mut matches: Vec<&str> = pool
        .iter()
        .map(String::as_str)
        .filter(|c| c.to_lowercase().starts_with(&lower))
        .collect();
    matches.sort_unstable();
    matches.dedup();
    let first = *matches.first()?;
    let common = matches.iter().fold(first.to_owned(), |acc, m| {
        acc.chars()
            .zip(m.chars())
            .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
            .map(|(a, _)| a)
            .collect()
    });
    let completed = if common.chars().count() > value.chars().count() {
        common
    } else {
        // Cannot grow: the first match in order.
        first.to_owned()
    };
    if completed.eq_ignore_ascii_case(value) {
        return None;
    }
    Some(format!("{}{}{key}{completed}", &text[..start], &token[..lead]))
}

#[derive(Clone, Default)]
struct CompletionCycle {
    last: String,
    candidates: Vec<String>,
    next: usize,
    frame: Option<u64>,
}

/// Candidate queries for the final token, sorted and deduplicated. Uses the same token parser
/// as `complete_token`, with each vocabulary entry in isolation.
#[must_use]
pub fn completion_candidates(text: &str, vocab: &CompletionVocab) -> Vec<String> {
    let mut out = Vec::new();
    for group in 0..4 {
        let pool = match group {
            0 => &vocab.keys,
            1 => &vocab.types,
            2 => &vocab.kinds,
            _ => &vocab.tags,
        };
        for candidate in pool {
            let mut one = CompletionVocab::default();
            match group {
                0 => one.keys.push(candidate.clone()),
                1 => one.types.push(candidate.clone()),
                2 => one.kinds.push(candidate.clone()),
                _ => one.tags.push(candidate.clone()),
            }
            if let Some(query) = complete_token(text, &one) {
                out.push(query);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Active candidates retained by `tab_complete` for this search box.
#[must_use]
pub fn active_completions(ctx: &egui::Context, id: egui::Id, text: &str) -> Vec<String> {
    ctx.data_mut(|data| data.get_temp::<CompletionCycle>(id.with("completion_cycle")))
        .filter(|cycle| cycle.last == text)
        .map_or_else(Vec::new, |cycle| cycle.candidates)
}

/// The completion hook of a search box: when `response`'s field has the focus and Tab is pressed,
/// completes `text` with [`complete_token`] and consumes the Tab (else Tab moves the focus as
/// usual). The Content Browser's search box calls it with [`CompletionVocab::content_browser`].
/// Whether the text changed.
pub fn tab_complete(
    ctx: &egui::Context,
    response: &egui::Response,
    text: &mut String,
    vocab: &CompletionVocab,
) -> bool {
    if !response.has_focus() || !ctx.input(|input| input.key_pressed(egui::Key::Tab)) {
        return false;
    }
    let id = response.id.with("completion_cycle");
    let mut cycle = ctx
        .data_mut(|data| data.get_temp::<CompletionCycle>(id))
        .unwrap_or_default();
    if cycle.last != *text {
        cycle = CompletionCycle::default();
    }
    let frame = ctx.cumulative_frame_nr();
    if cycle.frame == Some(frame) {
        return false;
    }
    if ctx.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab)) {
        cycle.last.clone_from(text);
        cycle.frame = Some(frame);
        ctx.data_mut(|data| data.insert_temp(id, cycle));
        ctx.memory_mut(|memory| memory.move_focus(egui::FocusDirection::Previous));
        return false;
    }
    let done = if cycle.candidates.is_empty() {
        let Some(done) = complete_token(text, vocab) else {
            return false;
        };
        cycle.candidates = completion_candidates(text, vocab);
        cycle.next = cycle
            .candidates
            .iter()
            .position(|c| *c == done)
            .map_or(0, |i| i + 1);
        done
    } else {
        let done = cycle.candidates[cycle.next % cycle.candidates.len()].clone();
        cycle.next += 1;
        done
    };
    if !ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)) {
        return false;
    }
    let changed = *text != done;
    *text = done;
    cycle.last.clone_from(text);
    cycle.frame = Some(frame);
    ctx.data_mut(|data| data.insert_temp(id, cycle));
    if let Some(mut state) = egui::TextEdit::load_state(ctx, response.id) {
        let end = egui::text::CCursor::new(text.chars().count());
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(end)));
        state.store(ctx, response.id);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(text: &str) -> AssetQuery {
        AssetQuery::parse(text).expect("query parses")
    }

    const PATHS: &[&str] = &[
        "assets/props/rock.glb",
        "assets/props/Rock_Large.glb",
        "assets/textures/rock_albedo.png",
        "assets/textures/moss.png",
        "assets/rocks/pebble.glb",
        "assets/mat/rock.material.ron",
        "assets/maps/arena.level.ron",
        "assets/props/crate.prefab.ron",
        "assets/sfx/rockfall.ogg",
    ];

    #[test]
    fn token_completion_preserves_unicode_whitespace() {
        assert_eq!(
            complete_token("stone\u{2003}type:te", &CompletionVocab::content_browser()).as_deref(),
            Some("stone\u{2003}type:texture")
        );
    }

    #[test]
    fn words_match_any_case_and_rank_names_first() {
        let found = search_paths(&query("ROCK"), PATHS.iter().copied());
        assert_eq!(
            found,
            [
                // Rank 0: the file name starts with the word.
                "assets/mat/rock.material.ron",
                "assets/props/Rock_Large.glb",
                "assets/props/rock.glb",
                "assets/sfx/rockfall.ogg",
                "assets/textures/rock_albedo.png",
                // Rank 2: only a folder name holds it.
                "assets/rocks/pebble.glb",
            ]
        );
    }

    #[test]
    fn every_word_must_match_and_minus_excludes() {
        assert_eq!(
            search_paths(&query("rock props"), PATHS.iter().copied()),
            ["assets/props/Rock_Large.glb", "assets/props/rock.glb"]
        );
        assert_eq!(
            search_paths(&query("rock -glb -ron"), PATHS.iter().copied()),
            ["assets/sfx/rockfall.ogg", "assets/textures/rock_albedo.png"]
        );
        // Rank 1: every word in the name, the first not at its start.
        assert_eq!(
            query("albedo rock").rank_path("assets/textures/rock_albedo.png"),
            Some(1)
        );
    }

    #[test]
    fn a_variant_searches_as_a_prefab() {
        let paths = ["assets/props/red_lamp.variant.ron", "assets/props/rock.glb"];
        assert_eq!(
            search_paths(&query("type:prefab"), paths.iter().copied()),
            ["assets/props/red_lamp.variant.ron"]
        );
    }

    #[test]
    fn type_and_ext_filters() {
        assert_eq!(
            search_paths(&query("type:mesh"), PATHS.iter().copied()),
            [
                "assets/props/Rock_Large.glb",
                "assets/props/rock.glb",
                "assets/rocks/pebble.glb"
            ]
        );
        assert_eq!(
            search_paths(&query("t:texture t:audio rock"), PATHS.iter().copied()),
            ["assets/sfx/rockfall.ogg", "assets/textures/rock_albedo.png"]
        );
        assert_eq!(
            search_paths(&query("ext:ron"), PATHS.iter().copied()),
            [
                "assets/maps/arena.level.ron",
                "assets/mat/rock.material.ron",
                "assets/props/crate.prefab.ron"
            ]
        );
        assert_eq!(
            search_paths(&query("ext:.material.ron"), PATHS.iter().copied()),
            ["assets/mat/rock.material.ron"]
        );
        // `ext:` is a suffix after a dot, not any suffix of the name.
        assert_eq!(query("ext:on").rank_path("assets/a.ron"), None);
        assert_eq!(
            query("ext:ron").rank_path("assets/ron"),
            None,
            "no dot, no suffix"
        );
        assert_eq!(asset_type("assets/T.PNG"), "texture");
        assert_eq!(asset_type("assets/a.matgraph.ron"), "material");
        assert_eq!(asset_type("assets/readme.txt"), "other");
    }

    #[test]
    fn bad_filters_name_their_code() {
        assert_eq!(
            AssetQuery::parse("type:sprite").map_err(|e| e.code),
            Err("search_bad_type")
        );
        for text in ["type:", "t:", "ext:", "ext:.", "rock -"] {
            assert_eq!(
                AssetQuery::parse(text).map_err(|e| e.code),
                Err("search_empty_filter"),
                "{text:?}"
            );
        }
        assert!(query("  ").is_empty(), "blank text is the empty query");
        assert_eq!(query("").rank_path("assets/anything.bin"), Some(0));
    }

    #[test]
    fn windows_separators_match_slash_words() {
        let windows = "assets\\props\\rock.glb";
        assert_eq!(query("props/rock").rank_path(windows), Some(2));
        assert_eq!(query("props\\rock").rank_path("assets/props/rock.glb"), Some(2));
        assert_eq!(
            search_key(windows).1,
            "assets/props/".len(),
            "name offset after the last separator"
        );
    }

    #[test]
    fn refinement_is_sound() {
        let pairs = [
            ("ro", "roc", true),
            ("ro", "rock", true),
            ("rock", "rock props", true),
            ("rock", "type:mesh rock", true),
            ("type:mesh type:texture", "type:mesh", true),
            ("ext:png", "ext:png rock", true),
            ("-glb", "-gl", true),
            ("rock -gl", "rock -glb", false),
            ("rock", "roc", false),
            ("type:mesh", "rock", false),
            ("type:mesh", "type:texture", false),
            ("", "anything", true),
            ("ext:png", "", false),
        ];
        for (older, newer, expected) in pairs {
            let (older_q, newer_q) = (query(older), query(newer));
            assert_eq!(newer_q.refines(&older_q), expected, "{newer:?} refines {older:?}");
            if expected {
                // Soundness on the sample: whatever the newer query matches, the older one does.
                for path in PATHS {
                    if newer_q.rank_path(path).is_some() {
                        assert!(
                            older_q.rank_path(path).is_some(),
                            "{newer:?} ⊄ {older:?} at {path}"
                        );
                    }
                }
            }
        }
    }

    fn facts(tags: &[(&str, &str)], meta: &[(&str, &str)], states: &[AssetState]) -> AssetFacts {
        let mut facts = AssetFacts::new(
            tags.iter().map(|(k, v)| (k.to_string(), v.to_string())),
            meta.iter().map(|(k, v)| (k.to_string(), v.to_string())),
        );
        for state in states {
            facts.set_state(*state, true);
        }
        facts
    }

    #[test]
    fn tag_state_and_meta_words_filter_through_the_facts() {
        let rock = facts(
            &[("biome", "Desert"), ("ext", "glb")],
            &[("tool", "agent"), ("model", "Sketch-2"), ("importer", "gltf")],
            &[AssetState::Review, AssetState::Unreferenced],
        );
        let (lower, at) = search_key("assets/props/rock.glb");
        let rank = |text: &str, facts: Option<&AssetFacts>| query(text).rank_with(&lower, at, facts);
        assert_eq!(rank("tag:biome", Some(&rock)), Some(0));
        assert_eq!(
            rank("tag:biome=desert", Some(&rock)),
            Some(0),
            "values match any case"
        );
        assert_eq!(rank("tag:biome=forest", Some(&rock)), None);
        assert_eq!(
            rank("tag:biome=forest tag:ext=glb", Some(&rock)),
            Some(0),
            "tag: words widen"
        );
        assert_eq!(rank("is:review", Some(&rock)), Some(0));
        assert_eq!(rank("is:modified", Some(&rock)), None);
        assert_eq!(
            rank("is:modified is:unreferenced", Some(&rock)),
            Some(0),
            "is: words widen"
        );
        assert_eq!(
            rank("meta:model=sketch", Some(&rock)),
            Some(0),
            "meta values match by part"
        );
        assert_eq!(
            rank("meta:model=sketch meta:tool=hand", Some(&rock)),
            None,
            "every meta: word"
        );
        assert_eq!(rank("meta:origin", Some(&rock)), None, "an unset field");
        assert_eq!(
            rank("rock tag:biome is:review", Some(&rock)),
            Some(0),
            "combines with words"
        );
        // Without facts the fact words match nothing; the plain path rank ignores them.
        assert_eq!(rank("tag:biome", None), None);
        assert_eq!(query("tag:biome").rank(&lower, at), None);
        assert_eq!(query("rock").rank(&lower, at), Some(0));
    }

    #[test]
    fn plain_words_match_tag_and_meta_values_after_paths() {
        let rock = facts(&[("biome", "Desert")], &[("origin", "kitbash-scan-07")], &[]);
        let (lower, at) = search_key("assets/props/rock.glb");
        let q = |text: &str| query(text).rank_with(&lower, at, Some(&rock));
        assert_eq!(q("desert"), Some(3), "a tag value: after every path rank");
        assert_eq!(q("KITBASH"), Some(3), "a .meta value, any case");
        assert_eq!(q("rock desert"), Some(3));
        assert_eq!(q("rock"), Some(0));
        assert_eq!(q("forest"), None);
        // The automatic tags repeat the path and are not free text.
        let auto = facts(&[("ext", "glb"), ("folder", "props")], &[], &[]);
        assert_eq!(query("props").rank_with("assets/x.png", 7, Some(&auto)), None);
    }

    #[test]
    fn new_filter_words_name_their_codes() {
        assert_eq!(
            AssetQuery::parse("is:dirty").map_err(|e| e.code),
            Err("search_bad_state")
        );
        for text in ["tag:", "is:", "meta:", "tag:=x", "meta:=x"] {
            assert_eq!(
                AssetQuery::parse(text).map_err(|e| e.code),
                Err("search_empty_filter"),
                "{text:?}"
            );
        }
        let parsed = query("tag:B tag:a=X is:problem meta:Model=Y");
        assert_eq!(
            parsed.tags,
            [("a".to_string(), Some("x".to_string())), ("b".to_string(), None)]
        );
        assert_eq!(parsed.states, [AssetState::Problem]);
        assert_eq!(parsed.meta, [("model".to_string(), Some("y".to_string()))]);
        assert!(parsed.uses_facts() && !parsed.is_empty());
        for state in AssetState::ALL {
            assert_eq!(AssetState::from_name(state.name()), Some(state));
        }
    }

    #[test]
    fn refinement_with_fact_words_is_sound() {
        let samples = [
            facts(
                &[("biome", "desert")],
                &[("model", "sketch-2")],
                &[AssetState::Review],
            ),
            facts(
                &[("biome", "forest")],
                &[],
                &[AssetState::Problem, AssetState::Review],
            ),
            facts(&[], &[("model", "sketch")], &[AssetState::Modified]),
            AssetFacts::default(),
        ];
        let pairs = [
            ("tag:biome", "tag:biome=desert", false),
            ("tag:biome=desert tag:biome=forest", "tag:biome=desert", true),
            ("", "tag:biome", true),
            ("tag:biome", "", false),
            ("is:review is:problem", "is:review", true),
            ("is:review", "is:review is:problem", false),
            ("meta:model", "meta:model=sk", true),
            ("meta:model=sk", "meta:model=sketch", true),
            ("meta:model=sketch", "meta:model=sk", false),
            ("meta:model", "rock", false),
            ("des", "desert", true),
        ];
        for (older, newer, expected) in pairs {
            let (older_q, newer_q) = (query(older), query(newer));
            assert_eq!(newer_q.refines(&older_q), expected, "{newer:?} refines {older:?}");
            if expected {
                for (path, facts) in PATHS.iter().zip(samples.iter().cycle()) {
                    let (lower, at) = search_key(path);
                    if newer_q.rank_with(&lower, at, Some(facts)).is_some() {
                        assert!(
                            older_q.rank_with(&lower, at, Some(facts)).is_some(),
                            "{newer:?} ⊄ {older:?} at {path}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn tab_completes_keys_types_kinds_and_tags() {
        let vocab = CompletionVocab {
            keys: ["a:", "e:", "type:", "t:", "ext:", "kind:", "tag:"]
                .map(str::to_owned)
                .to_vec(),
            types: ASSET_TYPES.iter().map(|t| (*t).to_owned()).collect(),
            kinds: ["none", "?", "light", "lamp_post"].map(str::to_owned).to_vec(),
            tags: ["loot", "locked"].map(str::to_owned).to_vec(),
        };
        let done = |text: &str| complete_token(text, &vocab);
        assert_eq!(done("rock ty").as_deref(), Some("rock type:"));
        assert_eq!(done("rock type:te").as_deref(), Some("rock type:texture"));
        assert_eq!(done("T:Me").as_deref(), Some("T:mesh"));
        assert_eq!(done("kind:li").as_deref(), Some("kind:light"));
        assert_eq!(
            done("kind:l").as_deref(),
            Some("kind:lamp_post"),
            "cannot grow past `l`: the first match"
        );
        assert_eq!(
            done("tag:lo").as_deref(),
            Some("tag:locked"),
            "cannot grow: first match"
        );
        assert_eq!(done("tag:locked"), None, "already complete");
        assert_eq!(done("tag:loo").as_deref(), Some("tag:loot"));
        assert_eq!(
            done("e:kind:li").as_deref(),
            Some("e:kind:light"),
            "after a provider prefix"
        );
        assert_eq!(done("rock "), None, "nothing to complete after a space");
        assert_eq!(done("zzz"), None);
        assert_eq!(done("ext:pn"), None, "ext: values are free text");
        let content = CompletionVocab::content_browser();
        assert_eq!(complete_token("rock e", &content).as_deref(), Some("rock ext:"));
        assert_eq!(complete_token("t:au", &content).as_deref(), Some("t:audio"));
    }
}
