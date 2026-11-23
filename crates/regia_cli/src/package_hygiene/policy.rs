// SPDX-License-Identifier: MPL-2.0
//! Project policy and bounded matcher compilation. Original Rust, ADR-8062.
//! Stable rule identity and allowance binding: ADR-8247.

use std::collections::BTreeSet;

use std::io::Read;
use std::path::Path;

use regex::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use super::{AuditError, Finding};

/// Finding rule id for an allowance whose stored definition does not match the rule it names.
pub const OBSOLETE_ALLOWANCE: &str = "allowance_obsolete";

/// BLAKE3 of a canonical rule definition (`kind` + body). The text is not recoverable from the digest.
///
/// `kind` is `string`, `pattern`, `term`, or `builtin`. Configurable allowances store this value
/// and keep waiving only while the named rule still has the same kind and body.
pub fn definition_digest(kind: &str, body: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"regia.package_hygiene.definition.v1\n");
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(body.as_bytes());
    format!("b3:{}", hasher.finalize().to_hex())
}

/// Configurable package checks, read from `[package_hygiene]` in `Regia.toml`.
///
/// Empty lists impose no project-specific naming rules. Secret signatures are enabled by
/// default. Unknown keys, empty patterns and invalid expressions fail closed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    /// Case-sensitive literals searched in raw bytes, including merged binary strings.
    pub forbidden_strings: Vec<String>,
    /// Rust byte-regex expressions searched in raw bytes (case-sensitive unless `(?i)`).
    pub forbidden_patterns: Vec<String>,
    /// `word:TEXT`, `sub:TEXT`, `re:PATTERN`, or bare case-insensitive substring terms.
    /// Terms inspect raw bytes and, when `scan_names` is true, relative entry names.
    /// Whole-word boundaries use ASCII letters, digits and underscore.
    pub deny_terms: Vec<String>,
    /// Also check file and directory names against deny terms (default true).
    pub scan_names: bool,
    /// Check private-key headers, webhook URLs and recognized token signatures (default true).
    pub scan_secrets: bool,
    /// Check email addresses in UTF-8 text (default false: public license contacts are common).
    pub scan_emails: bool,
    /// Explicit rules with stable ids. Empty means the three lists above supply the rules.
    /// A policy sets this table or those lists, not both.
    pub rules: Vec<Rule>,
    /// Narrow exceptions, each naming a rule ID, relative path glob and review reason.
    pub allow: Vec<Allowance>,
    /// What to do with absolute build paths in text assets. `rewrite` (default) replaces
    /// them with a relative tail during stage preparation. `reject` leaves the bytes and
    /// fails. `off` skips the check. Read-only audits never rewrite.
    pub path_scrub: PathScrub,
    /// Maximum bytes of one file or decompressed pak entry (default 1 GiB); excess fails.
    pub max_file_bytes: u64,
    /// Maximum total scanned file and decompressed-entry bytes (default 64 GiB); excess fails.
    pub max_total_bytes: u64,
    /// Peak bytes the audit may charge for its own buffers at once (default 32 MiB).
    /// A mapped pak is not charged; a decoded entry and a scan window are.
    pub max_memory_bytes: u64,
    /// Window used to stream literals and bounded patterns (default 1 MiB).
    /// Raised up to `max_memory_bytes` when a bounded pattern is wider than this.
    pub scan_window_bytes: u64,
    /// Sum of per-rule compiled-matcher reservations (default 8 MiB).
    /// A rule that would pass this total is refused before it is compiled.
    pub max_matcher_bytes: u64,
    /// Bytes the audit may present to matchers (default 1 TiB). Overlap is not charged twice.
    pub max_work: u64,
    /// Maximum visited entries, including directories and pak entries (default 100,000).
    pub max_entries: usize,
    /// Maximum findings retained before the audit refuses as incomplete (default 128).
    pub max_findings: usize,
    /// Final-artifact audit: deepest container opened inside the artifact (default 3, at most 16).
    /// The artifact is depth 0; a ZIP, tar, gzip or `.fpak` found in it is depth 1. A container
    /// past this depth fails closed with `depth`.
    pub max_container_depth: u32,
    /// Final-artifact audit: refuse an artifact that holds an encrypted entry or a container
    /// format this tool cannot open (default false: those are listed as uninspected).
    pub strict_containers: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            forbidden_strings: Vec::new(),
            forbidden_patterns: Vec::new(),
            deny_terms: Vec::new(),
            rules: Vec::new(),
            scan_names: true,
            scan_secrets: true,
            scan_emails: false,
            allow: Vec::new(),
            path_scrub: PathScrub::Rewrite,
            max_file_bytes: 1 << 30,
            max_total_bytes: 64 << 30,
            max_memory_bytes: Self::DEFAULT_MAX_MEMORY_BYTES,
            scan_window_bytes: Self::DEFAULT_SCAN_WINDOW_BYTES,
            max_matcher_bytes: Self::DEFAULT_MAX_MATCHER_BYTES,
            max_work: Self::DEFAULT_MAX_WORK,
            max_entries: 100_000,
            max_findings: 128,
            max_container_depth: Self::DEFAULT_MAX_CONTAINER_DEPTH,
            strict_containers: false,
        }
    }
}

/// How absolute build-machine paths in staged text assets are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PathScrub {
    /// Replace each absolute build path with the relative tail after the machine root.
    #[default]
    Rewrite,
    /// Leave the file unchanged and reject the package.
    Reject,
    /// Do not look for build paths.
    Off,
}

/// Stage layout rules from `[package]` in `Regia.toml`.
///
/// `stage_allow` is optional. When the key is absent, every staged file is allowed.
/// When the key is present, a file that matches no glob fails `stage_allow`.
/// `legal_notices` keeps matching notes that the notes drop would otherwise remove.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct StageRules {
    /// Slash-separated globs. `None` means the project set no allowlist.
    #[serde(default)]
    pub stage_allow: Option<Vec<String>>,
    /// Globs for notes that ship (licences, notices). Basename globs match at any depth.
    #[serde(default)]
    pub legal_notices: Vec<String>,
}

impl StageRules {
    /// Read `[package]` stage rules from the project manifest.
    ///
    /// A missing table allows every staged file and drops every internal note.
    /// Other manifest tables are ignored.
    ///
    /// # Errors
    /// Missing or unreadable manifest, invalid encoding, excessive size, or an invalid glob.
    pub fn from_project(project: &Path) -> Result<Self, AuditError> {
        Self::from_manifest(&read_manifest(project)?)
    }

    /// Parse the optional `[package]` table. Unknown keys in that table are ignored.
    ///
    /// # Errors
    /// Invalid TOML, a manifest over 1 MiB, or a glob the stage checker cannot compile.
    pub fn from_manifest(text: &str) -> Result<Self, AuditError> {
        if text.len() > 1 << 20 {
            return Err(AuditError::config("manifest_size"));
        }
        #[derive(Deserialize)]
        struct Manifest {
            #[serde(default)]
            package: StageRules,
        }
        let stage = toml::from_str::<Manifest>(text)
            .map_err(|_| AuditError::config("Regia.toml [package]"))?
            .package;
        stage.validate()?;
        Ok(stage)
    }

    /// Compile `stage_allow` and `legal_notices`.
    ///
    /// # Errors
    /// An empty pattern, an absolute pattern, or more than 1,024 globs in one list.
    pub fn validate(&self) -> Result<(), AuditError> {
        if let Some(globs) = &self.stage_allow {
            compile_globs(globs, "stage_allow")?;
        }
        compile_globs(&self.legal_notices, "legal_notices")?;
        Ok(())
    }
}

pub(super) fn read_manifest(project: &Path) -> Result<String, AuditError> {
    let path = regia_core::brand::project_manifest(project).unwrap_or_else(|| project.join("Regia.toml"));
    let file = std::fs::File::open(path).map_err(|_| AuditError::new("package_hygiene_io", "Regia.toml"))?;
    let mut text = String::new();
    file.take((1 << 20) + 1)
        .read_to_string(&mut text)
        .map_err(|_| AuditError::new("package_hygiene_io", "Regia.toml"))?;
    if text.len() > 1 << 20 {
        return Err(AuditError::config("manifest_size"));
    }
    Ok(text)
}

pub(super) fn is_stage_rule(id: &str) -> bool {
    matches!(id, "build_path" | "dev_flag" | "stage_allow" | "internal_notes")
}

pub(super) fn compile_globs(globs: &[String], field: &str) -> Result<Vec<Regex>, AuditError> {
    if globs.len() > 1024 {
        return Err(AuditError::config("rule_count"));
    }
    let mut compiled = Vec::with_capacity(globs.len());
    for (i, glob) in globs.iter().enumerate() {
        let n = i + 1;
        compiled.push(path_glob(glob).map_err(|_| AuditError::config(format!("{field}_{n}")))?);
    }
    Ok(compiled)
}

impl Policy {
    /// Default peak resident bytes for scan windows and decoded entries (32 MiB).
    pub const DEFAULT_MAX_MEMORY_BYTES: u64 = 32 << 20;
    /// Default streaming window for literals and bounded patterns (1 MiB).
    pub const DEFAULT_SCAN_WINDOW_BYTES: u64 = 1 << 20;
    /// Default aggregate compiled-matcher reservation (8 MiB).
    pub const DEFAULT_MAX_MATCHER_BYTES: u64 = 8 << 20;
    /// Default matcher-work budget, in bytes presented to matchers (1 TiB).
    pub const DEFAULT_MAX_WORK: u64 = 1 << 40;
    /// Default deepest nested container a final-artifact audit opens (3).
    pub const DEFAULT_MAX_CONTAINER_DEPTH: u32 = 3;
    /// Largest accepted `max_container_depth` (16).
    pub const MAX_CONTAINER_DEPTH_LIMIT: u32 = 16;

    /// The limits this policy will enforce, including defaults already filled in.
    #[must_use]
    pub fn limits(&self) -> super::AuditLimits {
        super::AuditLimits {
            max_memory_bytes: self.max_memory_bytes,
            max_file_bytes: self.max_file_bytes,
            max_total_bytes: self.max_total_bytes,
            max_work: self.max_work,
            scan_window_bytes: self.scan_window_bytes,
            max_matcher_bytes: self.max_matcher_bytes,
        }
    }
}

/// One configurable check with an id that does not depend on list position.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Stable id: an ASCII letter, then letters, digits or underscore, at most 64 characters.
    pub id: String,
    /// `string` (literal bytes), `pattern` (Rust byte-regex) or `term` (deny-term syntax).
    pub kind: String,
    /// Rule text. String and pattern bodies are matched as themselves; a term uses
    /// `word:`, `sub:`, `re:` or a bare substring, the same syntax as `deny_terms`.
    pub body: String,
}

impl Rule {
    /// Digest of this rule's kind and body. Allowances store the result, not `body`.
    #[must_use]
    pub fn definition(&self) -> String {
        definition_digest(&self.kind, &self.body)
    }
}

/// A reviewed exception for a specific rule and package-relative path.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Allowance {
    /// Exact rule ID, for example `deny_term_1` or `secret_email`; wildcards are not accepted.
    pub rule: String,
    /// Slash-separated glob: `*`/`?` stay within a component, `**` spans components.
    /// A pattern without a slash matches a basename at any depth.
    pub path: String,
    /// Nonblank review reason, kept in policy rather than emitted with scanned content.
    pub reason: String,
    /// Digest from [`definition_digest`] of the rule text this exception was reviewed against.
    /// Absent on a hand-built value means "the named rule as it is now". A loaded manifest
    /// stores the digest. A later body change leaves this digest in place and the exception
    /// becomes [`OBSOLETE_ALLOWANCE`] instead of following a different rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

/// One enabled rule as the audit and a later editor policy job see it. No rule text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveRule {
    /// Stable rule id, including built-in secret ids.
    pub id: String,
    /// `string`, `pattern`, `term` or `builtin`.
    pub kind: String,
    /// [`definition_digest`] of the enabled rule. Built-in digests hash the id, not the signature.
    pub definition: String,
}

/// One allowance that still matches its rule definition and path. No rule text and no reason text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveAllowance {
    /// Stable rule id.
    pub rule: String,
    /// Package-relative glob from the allowance.
    pub path: String,
    /// Definition digest the allowance is bound to.
    pub definition: String,
}

/// The policy the audit enforces, without configured literals, patterns or reasons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectivePolicy {
    /// Enabled rules, sorted by id.
    pub rules: Vec<EffectiveRule>,
    /// Allowances whose definition still matches the named rule, sorted by rule id then path.
    pub allowances: Vec<EffectiveAllowance>,
    /// `policy/allow/<index>/<rule-id>` for allowances that no longer match. Sorted.
    pub obsolete: Vec<String>,
    /// BLAKE3 of this effective policy. Hex with a `b3:` prefix. It contains no rule text.
    pub digest: String,
}

/// How a rule may be applied to a buffer without changing its first-match offset.
pub(super) enum MatchKind {
    /// Exact bytes. `ascii_case_insensitive` folds A–Z only, so the width stays `needle.len()`.
    Literal {
        needle: Vec<u8>,
        ascii_case_insensitive: bool,
    },
    /// ASCII whole word. The reported offset is the word, not its boundary byte.
    Word { needle: Vec<u8> },
    /// Every match is at most `width` bytes and the pattern has no anchors or lookaround.
    Bounded { width: usize },
    /// The match may be arbitrarily wide. Run on the whole input, or refuse.
    Unbounded,
}

pub(super) struct Matcher {
    pub id: String,
    pub regex: Regex,
    pub capture: bool,
    pub kind: MatchKind,
}

impl Matcher {
    pub fn offset(&self, bytes: &[u8]) -> Option<usize> {
        if self.capture {
            self.regex.captures(bytes)?.get(1).map(|m| m.start())
        } else {
            self.regex.find(bytes).map(|m| m.start())
        }
    }

    /// First match offset in a whole input (`bos` and `eos` are both true).
    pub fn find_in(&self, bytes: &[u8]) -> Option<usize> {
        self.find_in_window(bytes, true, true)
    }

    /// First match in one window. `bos`/`eos` are true only at the real start and end of the
    /// input. A word whose boundary byte is outside this window is deferred (`None`) instead of
    /// treating the window edge as a boundary or skipping ahead to a later word.
    pub fn find_in_window(&self, bytes: &[u8], bos: bool, eos: bool) -> Option<usize> {
        match &self.kind {
            MatchKind::Literal {
                needle,
                ascii_case_insensitive,
            } => find_literal(bytes, needle, *ascii_case_insensitive),
            MatchKind::Word { needle } => find_word(bytes, needle, bos, eos),
            MatchKind::Bounded { .. } | MatchKind::Unbounded => self.offset(bytes),
        }
    }

    /// Maximum match width when the rule can be streamed. `None` means whole-input or refuse.
    pub fn stream_width(&self) -> Option<usize> {
        match &self.kind {
            MatchKind::Literal { needle, .. } => Some(needle.len()),
            MatchKind::Word { needle } => Some(needle.len().saturating_add(2)),
            MatchKind::Bounded { width } => Some(*width),
            MatchKind::Unbounded => None,
        }
    }
}

fn find_literal(hay: &[u8], needle: &[u8], ascii_case_insensitive: bool) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|window| {
        if ascii_case_insensitive {
            window.eq_ignore_ascii_case(needle)
        } else {
            window == needle
        }
    })
}

fn is_ascii_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn find_word(hay: &[u8], needle: &[u8], bos: bool, eos: bool) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    for (start, window) in hay.windows(needle.len()).enumerate() {
        if !window.eq_ignore_ascii_case(needle) {
            continue;
        }
        let after = start + needle.len();
        // The previous window already judged a match that starts on the kept overlap's first
        // byte. A match that runs off the end of this window is left for the next window,
        // which still holds that tail, so a later word in this window is not reported first.
        if start == 0 && !bos {
            continue;
        }
        if after == hay.len() && !eos {
            return None;
        }
        let before_ok = start == 0 || !is_ascii_word(hay[start - 1]);
        let after_ok = after == hay.len() || !is_ascii_word(hay[after]);
        if before_ok && after_ok {
            return Some(start);
        }
    }
    None
}

pub(super) struct Compiled {
    pub raw: Vec<Matcher>,
    pub terms: Vec<Matcher>,
    pub email: Option<Matcher>,
    pub key_file: Option<Matcher>,
    pub allow: Vec<(String, Regex)>,
    /// Bytes reserved for compiled matchers. Never exceeds the policy cap.
    pub matcher_bytes: u64,
    /// Rules compiled successfully before a budget or syntax refusal.
    pub matchers_compiled: u32,
    pub digest: String,
    pub obsolete: Vec<Finding>,
    pub effective: EffectivePolicy,
}

/// Per-rule compiled-program cap. The aggregate cap is `Policy::max_matcher_bytes`.
const PER_RULE_SIZE_LIMIT: usize = 2 << 20;
/// Reservation charged for a literal before it is compiled (bytes of pattern, plus a fixed program).
const LITERAL_RESERVE_PER_BYTE: u64 = 64;
const LITERAL_RESERVE_FIXED: u64 = 256;
/// Reservation charged for a regex before it is compiled.
const REGEX_RESERVE_PER_BYTE: u64 = 128;
const REGEX_RESERVE_FIXED: u64 = 1024;

struct Budget {
    cap: u64,
    charged: u64,
    compiled: u32,
}

impl Budget {
    fn reserve_literal(&mut self, len: usize) -> Result<(), ()> {
        let estimate = (len as u64)
            .saturating_mul(LITERAL_RESERVE_PER_BYTE)
            .saturating_add(LITERAL_RESERVE_FIXED);
        self.reserve(estimate)
    }

    fn reserve_regex(&mut self, len: usize) -> Result<(), ()> {
        let estimate = (len as u64)
            .saturating_mul(REGEX_RESERVE_PER_BYTE)
            .saturating_add(REGEX_RESERVE_FIXED);
        self.reserve(estimate)
    }

    fn reserve(&mut self, estimate: u64) -> Result<(), ()> {
        let next = self.charged.saturating_add(estimate);
        if estimate == 0 || next > self.cap || next < self.charged {
            return Err(());
        }
        self.charged = next;
        Ok(())
    }

    fn compiled(&mut self) {
        self.compiled = self.compiled.saturating_add(1);
    }

    fn error(&self) -> AuditError {
        AuditError {
            code: "package_hygiene_config",
            location: "matcher_budget".into(),
            location_id: None,
            report: None,
            matcher_bytes: self.charged,
            matchers_compiled: self.compiled,
        }
    }
}

struct MatcherRequest<'a> {
    id: String,
    pattern: &'a str,
    unicode: bool,
    insensitive: bool,
    capture: bool,
    kind: MatchKind,
    literal: bool,
}

fn matcher(budget: &mut Budget, request: MatcherRequest<'_>) -> Result<Matcher, AuditError> {
    // Never echo patterns or parser errors: an owner can list an actual secret to forbid it.
    if request.pattern.is_empty() || request.pattern.len() > 16_384 {
        return Err(AuditError::config(request.id));
    }
    // Charge the reservation before `build`, and do not compile a rule that would pass the
    // aggregate cap. `size_limit` stays the per-rule ceiling.
    let reserved = if request.literal {
        budget.reserve_literal(request.pattern.len())
    } else {
        budget.reserve_regex(request.pattern.len())
    };
    if reserved.is_err() {
        return Err(budget.error());
    }
    let regex = RegexBuilder::new(request.pattern)
        .unicode(request.unicode)
        .case_insensitive(request.insensitive)
        .size_limit(PER_RULE_SIZE_LIMIT)
        .dfa_size_limit(PER_RULE_SIZE_LIMIT)
        .build()
        .map_err(|_| AuditError::config(&request.id))?;
    if regex.is_match(b"") {
        return Err(AuditError::config(request.id));
    }
    budget.compiled();
    Ok(Matcher {
        id: request.id,
        regex,
        capture: request.capture,
        kind: request.kind,
    })
}

fn limit_rejected(value: u64) -> bool {
    value == 0 || value == u64::MAX
}

/// Widest match of a pattern that can be searched in a window, or `None` when the pattern
/// may be wider, uses anchors, or uses syntax this classifier does not prove.
fn bounded_width(pattern: &str, unicode: bool) -> Option<usize> {
    let bytes = pattern.as_bytes();
    let mut parser = WidthParser {
        bytes,
        index: 0,
        unicode,
    };
    let width = parser.alternation()?;
    if parser.index != bytes.len() || width == 0 {
        return None;
    }
    Some(width)
}

struct WidthParser<'a> {
    bytes: &'a [u8],
    index: usize,
    unicode: bool,
}

impl WidthParser<'_> {
    fn alternation(&mut self) -> Option<usize> {
        let mut width = self.concat()?;
        while self.bytes.get(self.index) == Some(&b'|') {
            self.index += 1;
            width = width.max(self.concat()?);
        }
        Some(width)
    }

    fn concat(&mut self) -> Option<usize> {
        let mut width = 0usize;
        while let Some(byte) = self.bytes.get(self.index) {
            if *byte == b'|' || *byte == b')' {
                break;
            }
            width = width.checked_add(self.repeat()?)?;
        }
        Some(width)
    }

    fn repeat(&mut self) -> Option<usize> {
        let width = self.atom()?;
        let Some(byte) = self.bytes.get(self.index) else {
            return Some(width);
        };
        match byte {
            b'*' | b'+' => None,
            b'?' => {
                self.index += 1;
                Some(width)
            }
            b'{' => self.braces(width),
            _ => Some(width),
        }
    }

    fn braces(&mut self, atom: usize) -> Option<usize> {
        self.index += 1;
        let low = self.number()?;
        let high = if self.bytes.get(self.index) == Some(&b',') {
            self.index += 1;
            if self.bytes.get(self.index) == Some(&b'}') {
                return None;
            }
            self.number()?
        } else {
            low
        };
        if self.bytes.get(self.index) != Some(&b'}') || high < low {
            return None;
        }
        self.index += 1;
        if self.bytes.get(self.index) == Some(&b'?') {
            self.index += 1;
        }
        atom.checked_mul(high)
    }

    fn number(&mut self) -> Option<usize> {
        let start = self.index;
        while self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
            self.index += 1;
        }
        if self.index == start {
            return None;
        }
        std::str::from_utf8(&self.bytes[start..self.index])
            .ok()?
            .parse()
            .ok()
    }

    fn atom(&mut self) -> Option<usize> {
        let byte = *self.bytes.get(self.index)?;
        self.index += 1;
        match byte {
            b'(' => self.group(),
            b'[' => self.class(),
            b'.' => Some(self.char_width()),
            b'^' | b'$' | b'*' | b'+' | b'?' | b'|' | b')' | b'{' | b'}' => None,
            b'\\' => self.escape(),
            _ => Some(1),
        }
    }

    fn group(&mut self) -> Option<usize> {
        if self.bytes.get(self.index) == Some(&b'?') {
            self.index += 1;
            if self.bytes.get(self.index) != Some(&b':') {
                return None;
            }
            self.index += 1;
        }
        let width = self.alternation()?;
        if self.bytes.get(self.index) != Some(&b')') {
            return None;
        }
        self.index += 1;
        Some(width)
    }

    fn class(&mut self) -> Option<usize> {
        if self.bytes.get(self.index) == Some(&b'^') {
            self.index += 1;
        }
        let mut first = true;
        while let Some(byte) = self.bytes.get(self.index).copied() {
            if byte == b']' && !first {
                self.index += 1;
                return Some(self.char_width());
            }
            first = false;
            self.index += 1;
            if byte == b'\\' {
                self.escape_body()?;
            } else if byte == b'[' && self.bytes.get(self.index) == Some(&b':') {
                let close = self.bytes[self.index..]
                    .windows(2)
                    .position(|pair| pair == b":]")?;
                self.index += close + 2;
            }
        }
        None
    }

    fn escape(&mut self) -> Option<usize> {
        self.escape_body()
    }

    fn escape_body(&mut self) -> Option<usize> {
        let byte = *self.bytes.get(self.index)?;
        self.index += 1;
        match byte {
            b'A' | b'z' | b'b' | b'B' | b'p' | b'P' | b'k' => None,
            b'd' | b'D' | b's' | b'S' | b'w' | b'W' => Some(self.char_width()),
            b'a' | b'f' | b't' | b'n' | b'r' | b'v' => Some(1),
            b'x' => self.hex_escape(),
            b'u' => self.unicode_escape(),
            b'0'..=b'9' => None,
            other if other.is_ascii_alphanumeric() => None,
            _ => Some(1),
        }
    }

    fn hex_escape(&mut self) -> Option<usize> {
        if self.bytes.get(self.index) == Some(&b'{') {
            return self.unicode_escape_body();
        }
        self.hex_digits(2)?;
        Some(1)
    }

    fn unicode_escape(&mut self) -> Option<usize> {
        if self.bytes.get(self.index) != Some(&b'{') {
            return None;
        }
        self.unicode_escape_body()
    }

    fn unicode_escape_body(&mut self) -> Option<usize> {
        self.index += 1;
        let start = self.index;
        while self.bytes.get(self.index).is_some_and(u8::is_ascii_hexdigit) {
            self.index += 1;
        }
        if self.bytes.get(self.index) != Some(&b'}') || self.index == start {
            return None;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.index]).ok()?;
        let value = u32::from_str_radix(text, 16).ok()?;
        self.index += 1;
        if value <= 0x7F {
            Some(1)
        } else if self.unicode && value <= 0x10_FFFF {
            Some(4)
        } else {
            None
        }
    }

    fn hex_digits(&mut self, count: usize) -> Option<()> {
        let end = self.index.checked_add(count)?;
        if end > self.bytes.len() || !self.bytes[self.index..end].iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        self.index = end;
        Some(())
    }

    fn char_width(&self) -> usize {
        if self.unicode { 4 } else { 1 }
    }
}

fn kind_for_pattern(pattern: &str, unicode: bool) -> MatchKind {
    match bounded_width(pattern, unicode) {
        Some(width) => MatchKind::Bounded { width },
        None => MatchKind::Unbounded,
    }
}

fn term_kind(kind: &str, pattern: &str) -> MatchKind {
    let ascii = pattern.is_ascii();
    match kind {
        "word" if ascii => MatchKind::Word {
            needle: pattern.as_bytes().to_vec(),
        },
        "sub" if ascii => MatchKind::Literal {
            needle: pattern.as_bytes().to_vec(),
            ascii_case_insensitive: true,
        },
        "re" => kind_for_pattern(pattern, true),
        _ => MatchKind::Unbounded,
    }
}

impl Policy {
    /// Copy positional lists into [`Rule`] values when this policy has allowances, and fill any
    /// missing allowance digest from the rule that id names now.
    ///
    /// An allowance that already has a digest keeps it. Resolving a positional id uses the lists
    /// in this value (the policy as loaded). A digest that names a different body is not moved
    /// onto whatever rule now sits at that index.
    ///
    /// # Errors
    /// Duplicate or invalid ids, mixed list and explicit rules, an empty reason, an unknown
    /// unbound rule, or a rule that cannot be compiled.
    pub fn migrated(&self) -> Result<Self, AuditError> {
        let mut out = self.clone();
        let lists = !out.forbidden_strings.is_empty()
            || !out.forbidden_patterns.is_empty()
            || !out.deny_terms.is_empty();
        if out.rules.is_empty() && lists && !out.allow.is_empty() {
            out.rules = live_rules(&out)?;
            out.forbidden_strings.clear();
            out.forbidden_patterns.clear();
            out.deny_terms.clear();
        }
        out.bind_loaded()?;
        out.validate()?;
        Ok(out)
    }

    /// Enabled rules, live allowances and the policy digest. Bodies and reasons are omitted.
    ///
    /// # Errors
    /// The same failures as [`Self::migrated`] for a policy that cannot be compiled. A stale
    /// allowance is reported in [`EffectivePolicy::obsolete`] and is not an error.
    pub fn effective(&self) -> Result<EffectivePolicy, AuditError> {
        Ok(Compiled::new(self)?.effective)
    }

    pub(super) fn bind_loaded(&mut self) -> Result<(), AuditError> {
        let rules = live_rules(self)?;
        // Reasons are checked before any digest is written. The rule list and the builtin
        // flags are read first so the later assignment does not borrow `self` twice.
        for (index, allowance) in self.allow.iter().enumerate() {
            if allowance.reason.trim().is_empty() {
                return Err(allow_error(index));
            }
        }
        let mut fills = Vec::new();
        for (index, allowance) in self.allow.iter().enumerate() {
            if allowance.definition.is_some() {
                continue;
            }
            let digest = if let Some(rule) = rules.iter().find(|rule| rule.id == allowance.rule) {
                rule.definition()
            } else if let Some(digest) = enabled_builtin(self, &allowance.rule) {
                digest
            } else {
                return Err(allow_error(index));
            };
            fills.push((index, digest));
        }
        for (index, digest) in fills {
            self.allow[index].definition = Some(digest);
        }
        Ok(())
    }
}

impl Compiled {
    pub fn new(policy: &Policy) -> Result<Self, AuditError> {
        if limit_rejected(policy.max_file_bytes)
            || limit_rejected(policy.max_total_bytes)
            || limit_rejected(policy.max_memory_bytes)
            || limit_rejected(policy.scan_window_bytes)
            || limit_rejected(policy.max_matcher_bytes)
            || limit_rejected(policy.max_work)
            || policy.scan_window_bytes > policy.max_memory_bytes
            || policy.max_entries == 0
            || policy.max_findings == 0
            || policy.max_container_depth > Policy::MAX_CONTAINER_DEPTH_LIMIT
        {
            return Err(AuditError::config("limits"));
        }
        let rules = live_rules(policy)?;
        if rules.len() + policy.allow.len() > 1024 {
            return Err(AuditError::config("rule_count"));
        }
        let mut budget = Budget {
            cap: policy.max_matcher_bytes,
            charged: 0,
            compiled: 0,
        };
        let mut raw = Vec::new();
        let mut terms = Vec::new();
        let mut effective_rules = Vec::new();
        for rule in &rules {
            let compiled = compile_rule(&mut budget, rule)?;
            effective_rules.push(EffectiveRule {
                id: rule.id.clone(),
                kind: rule.kind.clone(),
                definition: rule.definition(),
            });
            if compiled.term {
                terms.push(compiled.matcher);
            } else {
                raw.push(compiled.matcher);
            }
        }
        if policy.scan_secrets {
            for (id, pattern) in SECRET_PATTERNS {
                raw.push(matcher(
                    &mut budget,
                    MatcherRequest {
                        id: (*id).into(),
                        pattern,
                        unicode: false,
                        insensitive: false,
                        capture: false,
                        kind: MatchKind::Unbounded,
                        literal: false,
                    },
                )?);
                effective_rules.push(builtin_rule(id));
            }
        }
        let email = policy
            .scan_emails
            .then(|| {
                matcher(
                    &mut budget,
                    MatcherRequest {
                        id: "secret_email".into(),
                        pattern: EMAIL_PATTERN,
                        unicode: false,
                        insensitive: false,
                        capture: false,
                        kind: MatchKind::Unbounded,
                        literal: false,
                    },
                )
            })
            .transpose()?;
        if email.is_some() {
            effective_rules.push(builtin_rule("secret_email"));
        }
        let key_file = policy
            .scan_secrets
            .then(|| {
                matcher(
                    &mut budget,
                    MatcherRequest {
                        id: "secret_key_file".into(),
                        pattern: KEY_FILE_PATTERN,
                        unicode: false,
                        insensitive: true,
                        capture: false,
                        kind: MatchKind::Unbounded,
                        literal: false,
                    },
                )
            })
            .transpose()?;
        if key_file.is_some() {
            effective_rules.push(builtin_rule("secret_key_file"));
        }
        effective_rules.sort_by(|left, right| left.id.cmp(&right.id));
        let mut allow = Vec::new();
        let mut obsolete = Vec::new();
        let mut live = Vec::new();
        for (index, allowance) in policy.allow.iter().enumerate() {
            if allowance.reason.trim().is_empty() {
                return Err(allow_error(index));
            }
            let Some(live_definition) = rule_definition_now(policy, &rules, &allowance.rule) else {
                if is_builtin_id(&allowance.rule) || allowance.definition.is_none() {
                    return Err(allow_error(index));
                }
                obsolete.push(obsolete_finding(index, &allowance.rule));
                continue;
            };
            let bound = allowance
                .definition
                .clone()
                .unwrap_or_else(|| live_definition.clone());
            if bound != live_definition {
                obsolete.push(obsolete_finding(index, &allowance.rule));
                continue;
            }
            let reserve = (allowance.path.len().saturating_mul(4).saturating_add(32)) as u64;
            if budget.reserve(reserve).is_err() {
                return Err(budget.error());
            }
            let glob = path_glob(&allowance.path).map_err(|_| AuditError {
                code: "package_hygiene_config",
                location: format!("allow_{}", index + 1),
                location_id: None,
                report: None,
                matcher_bytes: budget.charged,
                matchers_compiled: budget.compiled,
            })?;
            budget.compiled();
            live.push(EffectiveAllowance {
                rule: allowance.rule.clone(),
                path: allowance.path.clone(),
                definition: bound,
            });
            allow.push((allowance.rule.clone(), glob));
        }
        live.sort_by(|left, right| left.rule.cmp(&right.rule).then(left.path.cmp(&right.path)));
        obsolete.sort();
        let effective = EffectivePolicy {
            rules: effective_rules,
            allowances: live,
            obsolete: obsolete.iter().map(|finding| finding.path.clone()).collect(),
            digest: String::new(),
        };
        let digest = policy_digest(policy, &effective);
        let effective = EffectivePolicy {
            digest: digest.clone(),
            ..effective
        };
        Ok(Self {
            raw,
            terms,
            email,
            key_file,
            allow,
            matcher_bytes: budget.charged,
            matchers_compiled: budget.compiled,
            digest,
            obsolete,
            effective,
        })
    }

    pub fn allowed(&self, path: &str, rule: &str) -> bool {
        self.allow
            .iter()
            .any(|(id, glob)| id == rule && glob.is_match(path.as_bytes()))
    }
}

const EMAIL_PATTERN: &str = r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}";
const KEY_FILE_PATTERN: &str = r"(?:^|/)(?:dev\.key|id_(?:rsa|dsa|ecdsa|ed25519))$";

struct CompiledRule {
    matcher: Matcher,
    term: bool,
}

fn allow_error(index: usize) -> AuditError {
    AuditError::config(format!("allow_{}", index + 1))
}

fn valid_rule_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && id.len() <= 64
        && chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn is_builtin_id(id: &str) -> bool {
    id == "secret_email" || id == "secret_key_file" || SECRET_PATTERNS.iter().any(|(name, _)| *name == id)
}

fn enabled_builtin(policy: &Policy, id: &str) -> Option<String> {
    let enabled = if id == "secret_email" {
        policy.scan_emails
    } else if is_builtin_id(id) {
        policy.scan_secrets
    } else {
        false
    };
    enabled.then(|| definition_digest("builtin", id))
}

fn rule_definition_now(policy: &Policy, rules: &[Rule], id: &str) -> Option<String> {
    if let Some(rule) = rules.iter().find(|rule| rule.id == id) {
        return Some(rule.definition());
    }
    // Stage-preparation rules (ADR-8427) have no configurable body: an allowance naming one is
    // always live.
    if is_stage_rule(id) {
        return Some(definition_digest("stage", id));
    }
    enabled_builtin(policy, id)
}

fn lists_present(policy: &Policy) -> bool {
    !policy.forbidden_strings.is_empty()
        || !policy.forbidden_patterns.is_empty()
        || !policy.deny_terms.is_empty()
}

fn live_rules(policy: &Policy) -> Result<Vec<Rule>, AuditError> {
    if lists_present(policy) && !policy.rules.is_empty() {
        return Err(AuditError::config("rules"));
    }
    let rules = if policy.rules.is_empty() {
        synthesized(policy)
    } else {
        policy.rules.clone()
    };
    let mut seen = BTreeSet::new();
    for rule in &rules {
        check_rule(rule)?;
        if !seen.insert(rule.id.as_str()) || is_builtin_id(&rule.id) {
            return Err(AuditError::config("duplicate_rule_id"));
        }
    }
    Ok(rules)
}

fn synthesized(policy: &Policy) -> Vec<Rule> {
    let mut rules = Vec::new();
    push_list(
        &mut rules,
        "string",
        "forbidden_string",
        &policy.forbidden_strings,
    );
    push_list(
        &mut rules,
        "pattern",
        "forbidden_pattern",
        &policy.forbidden_patterns,
    );
    push_list(&mut rules, "term", "deny_term", &policy.deny_terms);
    rules
}

fn push_list(rules: &mut Vec<Rule>, kind: &str, prefix: &str, bodies: &[String]) {
    for (index, body) in bodies.iter().enumerate() {
        rules.push(Rule {
            id: format!("{prefix}_{}", index + 1),
            kind: kind.to_owned(),
            body: body.clone(),
        });
    }
}

fn check_rule(rule: &Rule) -> Result<(), AuditError> {
    if !valid_rule_id(&rule.id) {
        return Err(AuditError::config("rule_id"));
    }
    if !matches!(rule.kind.as_str(), "string" | "pattern" | "term") {
        return Err(AuditError::config("rule_kind"));
    }
    if rule.body.is_empty() || rule.body.len() > 16_384 {
        return Err(AuditError::config(format!("rule_body:{}", rule.id)));
    }
    Ok(())
}

fn compile_rule(budget: &mut Budget, rule: &Rule) -> Result<CompiledRule, AuditError> {
    match rule.kind.as_str() {
        "string" => {
            let pattern = regex::escape(&rule.body);
            let compiled = matcher(
                budget,
                MatcherRequest {
                    id: rule.id.clone(),
                    pattern: &pattern,
                    unicode: false,
                    insensitive: false,
                    capture: false,
                    kind: MatchKind::Literal {
                        needle: rule.body.as_bytes().to_vec(),
                        ascii_case_insensitive: false,
                    },
                    literal: true,
                },
            )?;
            Ok(CompiledRule {
                matcher: compiled,
                term: false,
            })
        }
        "pattern" => {
            let compiled = matcher(
                budget,
                MatcherRequest {
                    id: rule.id.clone(),
                    pattern: &rule.body,
                    unicode: false,
                    insensitive: false,
                    capture: false,
                    kind: kind_for_pattern(&rule.body, false),
                    literal: false,
                },
            )?;
            Ok(CompiledRule {
                matcher: compiled,
                term: false,
            })
        }
        "term" => {
            let (expression, insensitive, capture) =
                term_expression(&rule.body).map_err(|_| AuditError::config(rule.id.clone()))?;
            let (kind_name, pattern_text) = term_parts(&rule.body);
            let compiled = matcher(
                budget,
                MatcherRequest {
                    id: rule.id.clone(),
                    pattern: &expression,
                    unicode: true,
                    insensitive,
                    capture,
                    kind: term_kind(&kind_name, pattern_text),
                    literal: kind_name == "sub" && pattern_text.is_ascii(),
                },
            )?;
            Ok(CompiledRule {
                matcher: compiled,
                term: true,
            })
        }
        _ => Err(AuditError::config("rule_kind")),
    }
}

fn term_parts(body: &str) -> (String, &str) {
    let (kind, pattern) = body
        .split_once(':')
        .filter(|(kind, _)| matches!(kind.to_ascii_lowercase().as_str(), "word" | "sub" | "re"))
        .unwrap_or(("sub", body));
    (kind.to_ascii_lowercase(), pattern)
}

fn term_expression(body: &str) -> Result<(String, bool, bool), ()> {
    let (kind, pattern) = body
        .split_once(':')
        .filter(|(kind, _)| matches!(kind.to_ascii_lowercase().as_str(), "word" | "sub" | "re"))
        .unwrap_or(("sub", body));
    if pattern.is_empty() || pattern.len() > 16_384 {
        return Err(());
    }
    let kind = kind.to_ascii_lowercase();
    let expression = match kind.as_str() {
        "word" => format!(r"(?:^|(?-u:\W))({})(?:$|(?-u:\W))", regex::escape(pattern)),
        "sub" => regex::escape(pattern),
        _ => pattern.to_owned(),
    };
    Ok((expression, kind != "re", kind == "word"))
}

fn builtin_rule(id: &str) -> EffectiveRule {
    EffectiveRule {
        id: id.to_owned(),
        kind: "builtin".to_owned(),
        definition: definition_digest("builtin", id),
    }
}

fn builtin_pattern_stamp(id: &str) -> String {
    if let Some(pattern) = SECRET_PATTERNS
        .iter()
        .find(|(name, _)| *name == id)
        .map(|(_, pattern)| *pattern)
    {
        return definition_digest("builtin-pattern", pattern);
    }
    match id {
        "secret_email" => definition_digest("builtin-pattern", EMAIL_PATTERN),
        "secret_key_file" => definition_digest("builtin-pattern", KEY_FILE_PATTERN),
        _ => String::new(),
    }
}

fn obsolete_path(index: usize, rule: &str) -> String {
    if valid_rule_id(rule) {
        format!("policy/allow/{index}/{rule}")
    } else {
        format!("policy/allow/{index}")
    }
}

fn obsolete_finding(index: usize, rule: &str) -> Finding {
    Finding {
        path: obsolete_path(index, rule),
        location_id: format!("policy-{index}"),
        rule: OBSOLETE_ALLOWANCE.to_owned(),
        source: "policy".to_owned(),
        offset: index,
    }
}

fn policy_digest(policy: &Policy, effective: &EffectivePolicy) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"regia.package_hygiene.policy.v1\n");
    for rule in &effective.rules {
        let line = format!(
            "rule {} {} {} {}\n",
            rule.id,
            rule.kind,
            rule.definition,
            builtin_pattern_stamp(&rule.id)
        );
        hasher.update(line.as_bytes());
    }
    for allowance in &effective.allowances {
        let line = format!(
            "allow {} {} {}\n",
            allowance.rule, allowance.definition, allowance.path
        );
        hasher.update(line.as_bytes());
    }
    let flags = format!(
        "flag names {} secrets {} emails {}\nlimit file {} total {} entries {} findings {}\n",
        u8::from(policy.scan_names),
        u8::from(policy.scan_secrets),
        u8::from(policy.scan_emails),
        policy.max_file_bytes,
        policy.max_total_bytes,
        policy.max_entries,
        policy.max_findings
    );
    hasher.update(flags.as_bytes());
    format!("b3:{}", hasher.finalize().to_hex())
}

// Signatures describe secret-shaped data, not validity or ownership of credentials.
const SECRET_PATTERNS: &[(&str, &str)] = &[
    (
        "secret_private_key",
        r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----",
    ),
    (
        "secret_webhook",
        r#"(?i)(?:https?://)?(?:discord(?:app)?\.com/api/webhooks/[0-9]+|hooks\.slack\.com/services/[A-Za-z0-9]+/[A-Za-z0-9]+/[A-Za-z0-9]+)[^\s\x00"<>]*"#,
    ),
    ("secret_aws_access_key", r"(?:AKIA|ASIA)[A-Z0-9]{16}"),
    (
        "secret_github_token",
        r"(?:gh[pousr]_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{22,})",
    ),
    ("secret_api_key", r"sk-[A-Za-z0-9_-]{20,}"),
    ("secret_slack_token", r"xox[abp]-[A-Za-z0-9-]{10,}"),
    (
        "secret_bot_token",
        r"[Bb]ot [A-Za-z0-9_-]{24}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]+",
    ),
];

pub(super) fn path_glob(pattern: &str) -> Result<Regex, ()> {
    if pattern.is_empty()
        || pattern.len() > 4096
        || pattern.starts_with('/')
        || pattern.as_bytes().get(1) == Some(&b':')
        || pattern.contains('\\')
        || pattern.split('/').any(|p| matches!(p, "." | ".."))
    {
        return Err(());
    }
    let mut expression = if pattern.contains('/') {
        String::from("^")
    } else {
        String::from("^(?:.*/)?")
    };
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    expression.push_str("(?:.*/)?");
                } else {
                    expression.push_str(".*");
                }
            }
            '*' => expression.push_str("[^/]*"),
            '?' => expression.push_str("[^/]"),
            c => expression.push_str(&regex::escape(&c.to_string())),
        }
    }
    expression.push('$');
    RegexBuilder::new(&expression)
        .unicode(true)
        .size_limit(2 << 20)
        .build()
        .map_err(|_| ())
}
