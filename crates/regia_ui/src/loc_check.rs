// SPDX-License-Identifier: MPL-2.0
//! Localization validator (`docs/specs/content-data.md` § 1.6, ADR-1466).
//!
//! [`parse_locale`] reads one locale's `.ftl` files into a [`ParsedLocale`]: every message with its
//! file and line, the `$variables` it uses, the messages it references and its select expressions.
//! [`validate`] compares the locales with the source locale (and, when given, with the keys the
//! gather tool found in content and code) and returns [`Finding`]s with stable codes.
//!
//! Plural checks use Regia's CLDR table ([`crate::plural`]). It agrees with the Fluent runtime
//! except the Arabic `n % 100` case and the Belarusian integer `few` case in
//! `docs/specs/content-data.md` § 1.5.

use std::collections::{BTreeMap, BTreeSet};

use fluent_syntax::ast;

use crate::plural::{self, PluralCategory};

/// How bad a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Breaks text a player sees (a missing key, a parse error, an unknown variable).
    Error,
    /// Probably wrong; `--strict` treats it as an error.
    Warning,
    /// A summary line.
    Info,
}

impl Severity {
    /// `"error"`, `"warning"` or `"info"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }
}

/// One validator finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Stable code (`loc_missing_key`, ...; spec § 1.6).
    pub code: &'static str,
    /// Severity.
    pub severity: Severity,
    /// The locale the finding is about.
    pub locale: String,
    /// The message key, when the finding is about one message.
    pub key: Option<String>,
    /// The file, when known.
    pub file: Option<String>,
    /// The 1-based line, when known.
    pub line: Option<u32>,
    /// A sentence for people.
    pub message: String,
}

/// One select expression of a message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SelectorInfo {
    /// The variable the selector reads (`$count`, also through `NUMBER($count)`).
    pub variable: Option<String>,
    /// Every variant key as written (`one`, `few`, `0`).
    pub keys: Vec<String>,
    /// The default variant's key.
    pub default: String,
    /// The selector is `NUMBER(…, type: "ordinal")`: its keys are ordinal plural categories
    /// (ADR-3359), so the checks compare them with the locale's ordinal categories.
    pub ordinal: bool,
}

impl SelectorInfo {
    /// Whether the selector picks by plural category: some non-default variant key other than
    /// `other` is a CLDR category name.
    #[must_use]
    pub fn is_plural(&self) -> bool {
        self.keys
            .iter()
            .any(|k| k != "other" && PluralCategory::parse(k).is_some())
    }
}

/// One message of a locale.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MessageInfo {
    /// The message id.
    pub key: String,
    /// The file it was read from.
    pub file: String,
    /// Its 1-based line (0 when the id could not be found again in the text).
    pub line: u32,
    /// Whether it has a value (a message with only attributes has none).
    pub has_value: bool,
    /// Attribute names.
    pub attributes: Vec<String>,
    /// `$variables` used anywhere in the value and attributes.
    pub variables: BTreeSet<String>,
    /// Other messages it references (`{ other-key }`).
    pub references: BTreeSet<String>,
    /// Its select expressions, in order.
    pub selectors: Vec<SelectorInfo>,
}

/// One locale's parsed files.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedLocale {
    /// The locale tag.
    pub locale: String,
    /// Messages by key (a key defined twice keeps the later definition, as the runtime does).
    pub messages: BTreeMap<String, MessageInfo>,
    /// Parse errors and duplicate keys found while reading.
    pub findings: Vec<Finding>,
}

/// The 1-based line of byte offset `pos` in `text`.
fn line_of(text: &str, pos: usize) -> u32 {
    let end = pos.min(text.len());
    let newlines = text.as_bytes()[..end].iter().filter(|&&b| b == b'\n').count();
    u32::try_from(newlines + 1).unwrap_or(u32::MAX)
}

/// The line of each `id =` at the start of a line (messages and terms), first occurrence order.
fn entry_lines(text: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let id: String = line
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if id.is_empty() || !id.starts_with(|c: char| c.is_ascii_alphabetic()) {
            continue;
        }
        if line[id.len()..].trim_start().starts_with('=') {
            out.push((id, u32::try_from(n + 1).unwrap_or(u32::MAX)));
        }
    }
    out
}

#[derive(Default)]
struct Walk {
    variables: BTreeSet<String>,
    references: BTreeSet<String>,
    selectors: Vec<SelectorInfo>,
}

impl Walk {
    fn pattern(&mut self, pattern: &ast::Pattern<&str>) {
        for element in &pattern.elements {
            if let ast::PatternElement::Placeable { expression } = element {
                self.expression(expression);
            }
        }
    }

    fn expression(&mut self, expression: &ast::Expression<&str>) {
        match expression {
            ast::Expression::Inline(inline) => self.inline(inline),
            ast::Expression::Select { selector, variants } => {
                self.inline(selector);
                let mut info = SelectorInfo {
                    variable: selector_variable(selector),
                    ordinal: selector_is_ordinal(selector),
                    ..SelectorInfo::default()
                };
                for variant in variants {
                    let key = match &variant.key {
                        ast::VariantKey::Identifier { name } => (*name).to_owned(),
                        ast::VariantKey::NumberLiteral { value } => (*value).to_owned(),
                    };
                    if variant.default {
                        info.default.clone_from(&key);
                    }
                    info.keys.push(key);
                    self.pattern(&variant.value);
                }
                self.selectors.push(info);
            }
        }
    }

    fn inline(&mut self, inline: &ast::InlineExpression<&str>) {
        match inline {
            ast::InlineExpression::VariableReference { id } => {
                self.variables.insert(id.name.to_owned());
            }
            ast::InlineExpression::MessageReference { id, .. } => {
                self.references.insert(id.name.to_owned());
            }
            ast::InlineExpression::FunctionReference { arguments, .. } => self.arguments(arguments),
            ast::InlineExpression::TermReference { arguments, .. } => {
                if let Some(arguments) = arguments {
                    self.arguments(arguments);
                }
            }
            ast::InlineExpression::Placeable { expression } => self.expression(expression),
            ast::InlineExpression::StringLiteral { .. } | ast::InlineExpression::NumberLiteral { .. } => {}
        }
    }

    fn arguments(&mut self, arguments: &ast::CallArguments<&str>) {
        for positional in &arguments.positional {
            self.inline(positional);
        }
        for named in &arguments.named {
            self.inline(&named.value);
        }
    }
}

/// `NUMBER(…, type: "ordinal")`: the run time selects ordinal categories for it (ADR-3359).
fn selector_is_ordinal(selector: &ast::InlineExpression<&str>) -> bool {
    matches!(selector, ast::InlineExpression::FunctionReference { id, arguments }
        if id.name == crate::loc_format::NUMBER_FUNCTION
            && arguments.named.iter().any(|n| n.name.name == "type"
                && matches!(n.value, ast::InlineExpression::StringLiteral { value } if value == "ordinal")))
}

/// `$x` or `FUNC($x, ...)`.
fn selector_variable(selector: &ast::InlineExpression<&str>) -> Option<String> {
    match selector {
        ast::InlineExpression::VariableReference { id } => Some(id.name.to_owned()),
        ast::InlineExpression::FunctionReference { arguments, .. } => {
            arguments.positional.first().and_then(selector_variable)
        }
        _ => None,
    }
}

/// Parses one locale's files (`(path, text)` pairs, in the order the runtime combines them).
#[must_use]
pub fn parse_locale(locale: &str, files: &[(String, String)]) -> ParsedLocale {
    let mut out = ParsedLocale {
        locale: locale.to_owned(),
        ..ParsedLocale::default()
    };
    for (file, text) in files {
        let resource = match fluent_syntax::parser::parse(text.as_str()) {
            Ok(resource) => resource,
            Err((resource, errors)) => {
                for error in errors {
                    out.findings.push(Finding {
                        code: "loc_parse_error",
                        severity: Severity::Error,
                        locale: locale.to_owned(),
                        key: None,
                        file: Some(file.clone()),
                        line: Some(line_of(text, error.pos.start)),
                        message: format!("does not parse: {:?}", error.kind),
                    });
                }
                resource
            }
        };
        let mut lines = entry_lines(text).into_iter();
        for entry in &resource.body {
            let ast::Entry::Message(message) = entry else {
                continue;
            };
            let key = message.id.name.to_owned();
            let line = lines.find(|(id, _)| *id == key).map_or(0, |(_, line)| line);
            let mut walk = Walk::default();
            if let Some(value) = &message.value {
                walk.pattern(value);
            }
            for attribute in &message.attributes {
                walk.pattern(&attribute.value);
            }
            if let Some(previous) = out.messages.get(&key) {
                out.findings.push(Finding {
                    code: "loc_duplicate_key",
                    severity: Severity::Warning,
                    locale: locale.to_owned(),
                    key: Some(key.clone()),
                    file: Some(file.clone()),
                    line: Some(line),
                    message: format!(
                        "`{key}` is defined again (first at {}:{}); the later one wins",
                        previous.file, previous.line
                    ),
                });
            }
            out.messages.insert(
                key.clone(),
                MessageInfo {
                    key,
                    file: file.clone(),
                    line,
                    has_value: message.value.is_some(),
                    attributes: message.attributes.iter().map(|a| a.id.name.to_owned()).collect(),
                    variables: walk.variables,
                    references: walk.references,
                    selectors: walk.selectors,
                },
            );
        }
    }
    out
}

/// What [`validate`] compares against.
#[derive(Debug, Clone, Default)]
pub struct CheckOptions {
    /// The source locale (keys are authored there). Default `en`.
    pub source: String,
    /// Extra fallback edges (`assets/localization/localization.ron` `fallbacks`).
    pub fallbacks: BTreeMap<String, Vec<String>>,
    /// The keys content and code use (from the gather tool), when known. `None` skips the
    /// `loc_unused_key` and gathered `loc_missing_key` checks.
    pub used_keys: Option<BTreeSet<String>>,
}

/// Per-locale counts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocaleSummary {
    /// The locale.
    pub locale: String,
    /// Messages it defines.
    pub keys: usize,
    /// Keys missing from its whole fallback chain.
    pub missing: usize,
    /// Source keys it lacks but a fallback has.
    pub untranslated: usize,
    /// Keys the source locale does not have.
    pub extra: usize,
    /// The fallback chain used.
    pub chain: Vec<String>,
}

/// The validator's answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    /// Every finding: by locale, then key, then code.
    pub findings: Vec<Finding>,
    /// One summary per locale, sorted.
    pub locales: Vec<LocaleSummary>,
}

impl Report {
    /// Findings of `severity`.
    pub fn count(&self, severity: Severity) -> usize {
        self.findings.iter().filter(|f| f.severity == severity).count()
    }

    /// Whether the report fails a check: any error, or any warning when `strict`.
    #[must_use]
    pub fn fails(&self, strict: bool) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity == Severity::Error || (strict && f.severity == Severity::Warning))
    }

    /// The findings with `code`.
    pub fn with_code<'a>(&'a self, code: &'a str) -> impl Iterator<Item = &'a Finding> + 'a {
        self.findings.iter().filter(move |f| f.code == code)
    }
}

fn finding(
    code: &'static str,
    severity: Severity,
    locale: &str,
    message: Option<&MessageInfo>,
    key: &str,
    text: String,
) -> Finding {
    Finding {
        code,
        severity,
        locale: locale.to_owned(),
        key: Some(key.to_owned()),
        file: message.map(|m| m.file.clone()),
        line: message.map(|m| m.line),
        message: text,
    }
}

/// Checks `locales` against the source locale and the used keys (spec § 1.6).
#[must_use]
pub fn validate(locales: &[ParsedLocale], options: &CheckOptions) -> Report {
    let source_tag = if options.source.is_empty() {
        "en"
    } else {
        options.source.as_str()
    };
    let by_tag: BTreeMap<&str, &ParsedLocale> = locales.iter().map(|l| (l.locale.as_str(), l)).collect();
    let loaded: BTreeSet<String> = by_tag.keys().map(|k| (*k).to_owned()).collect();
    let empty = ParsedLocale::default();
    let source = by_tag.get(source_tag).copied().unwrap_or(&empty);

    // Keys every locale should resolve: the source's messages plus what content and code use.
    let mut expected: BTreeSet<&str> = source.messages.keys().map(String::as_str).collect();
    if let Some(used) = &options.used_keys {
        expected.extend(used.iter().map(String::as_str));
    }

    let mut report = Report::default();
    for parsed in locales {
        report.findings.extend(parsed.findings.iter().cloned());
    }
    // Unused: source keys nothing names (references between messages count as uses).
    if let Some(used) = &options.used_keys {
        let referenced: BTreeSet<&str> = source
            .messages
            .values()
            .flat_map(|m| m.references.iter().map(String::as_str))
            .collect();
        for (key, message) in &source.messages {
            if !used.contains(key) && !referenced.contains(key.as_str()) {
                report.findings.push(finding(
                    "loc_unused_key",
                    Severity::Warning,
                    source_tag,
                    Some(message),
                    key,
                    format!("`{key}` is not used by any content or code the gather tool read"),
                ));
            }
        }
    }

    for parsed in locales {
        let tag = parsed.locale.as_str();
        let chain = crate::localization::fallback_chain(tag, &loaded, &options.fallbacks, source_tag);
        let mut summary = LocaleSummary {
            locale: tag.to_owned(),
            keys: parsed.messages.len(),
            chain: chain.clone(),
            ..LocaleSummary::default()
        };
        for key in &expected {
            if parsed.messages.contains_key(*key) {
                continue;
            }
            let provider = chain.iter().find(|l| {
                by_tag
                    .get(l.as_str())
                    .is_some_and(|p| p.messages.contains_key(*key))
            });
            match provider {
                None => {
                    summary.missing += 1;
                    report.findings.push(finding(
                        "loc_missing_key",
                        Severity::Error,
                        tag,
                        None,
                        key,
                        format!("`{key}` is missing from {tag} and its whole fallback chain {chain:?}"),
                    ));
                }
                Some(fallback) if tag != source_tag => {
                    summary.untranslated += 1;
                    report.findings.push(finding(
                        "loc_untranslated",
                        Severity::Warning,
                        tag,
                        None,
                        key,
                        format!("`{key}` is not translated in {tag}; players see the {fallback} text"),
                    ));
                }
                Some(_) => {}
            }
        }
        if tag != source_tag {
            for (key, message) in &parsed.messages {
                let Some(original) = source.messages.get(key) else {
                    summary.extra += 1;
                    report.findings.push(finding(
                        "loc_extra_key",
                        Severity::Warning,
                        tag,
                        Some(message),
                        key,
                        format!("`{key}` is in {tag} but not in the source locale {source_tag}"),
                    ));
                    continue;
                };
                for added in message.variables.difference(&original.variables) {
                    report.findings.push(finding(
                        "loc_variable_mismatch",
                        Severity::Error,
                        tag,
                        Some(message),
                        key,
                        format!("`{key}` uses ${added}, which the {source_tag} message does not pass"),
                    ));
                }
                for dropped in original.variables.difference(&message.variables) {
                    report.findings.push(finding(
                        "loc_variable_mismatch",
                        Severity::Warning,
                        tag,
                        Some(message),
                        key,
                        format!("`{key}` does not show ${dropped}, which the {source_tag} message uses"),
                    ));
                }
            }
        }
        plural_findings(parsed, &mut report.findings);
        if summary.missing > 0 || summary.extra > 0 {
            report.findings.push(Finding {
                code: "loc_key_set_differs",
                severity: Severity::Info,
                locale: tag.to_owned(),
                key: None,
                file: None,
                line: None,
                message: format!(
                    "{tag}: {} key(s) missing, {} extra compared with {source_tag}",
                    summary.missing, summary.extra
                ),
            });
        }
        report.locales.push(summary);
    }
    self_name_findings(locales, &mut report.findings);
    cycle_findings(&options.fallbacks, &mut report.findings);
    report.findings.sort_by(|a, b| {
        (a.locale.as_str(), a.key.as_deref(), a.code, a.line).cmp(&(
            b.locale.as_str(),
            b.key.as_deref(),
            b.code,
            b.line,
        ))
    });
    report.locales.sort_by(|a, b| a.locale.cmp(&b.locale));
    report
}

/// `loc_no_self_name` (ADR-1618): once any locale defines the `locale-name` message (the locale's
/// own name for language menus), every locale that lacks it gets a warning. A project whose
/// locales define none has no language menu to fill, so it gets no warning.
fn self_name_findings(locales: &[ParsedLocale], out: &mut Vec<Finding>) {
    let key = crate::locale_startup::SELF_NAME_KEY;
    if !locales.iter().any(|l| l.messages.contains_key(key)) {
        return;
    }
    for parsed in locales.iter().filter(|l| !l.messages.contains_key(key)) {
        out.push(Finding {
            code: "loc_no_self_name",
            severity: Severity::Warning,
            locale: parsed.locale.clone(),
            key: Some(key.to_owned()),
            file: None,
            line: None,
            message: format!(
                "{} does not define `{key}`; a language menu has no name to show for it",
                parsed.locale
            ),
        });
    }
}

/// `loc_fallback_cycle`: the extra fallback edges of `localization.ron` lead back to a locale on
/// their own path (`a` → `b` → `a`). The chain walk skips repeats, so a cycle never loops, but it
/// is almost always a mistake. One finding per cycle, on its smallest locale tag.
fn cycle_findings(edges: &BTreeMap<String, Vec<String>>, out: &mut Vec<Finding>) {
    fn walk(
        node: &str,
        edges: &BTreeMap<String, Vec<String>>,
        path: &mut Vec<String>,
        done: &mut BTreeSet<String>,
        cycles: &mut BTreeSet<Vec<String>>,
    ) {
        if let Some(at) = path.iter().position(|p| p == node) {
            let mut cycle = path[at..].to_vec();
            let first = cycle
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.cmp(b.1))
                .map_or(0, |(i, _)| i);
            cycle.rotate_left(first);
            cycles.insert(cycle);
            return;
        }
        if !done.insert(node.to_owned()) {
            return;
        }
        path.push(node.to_owned());
        for next in edges.get(node).into_iter().flatten() {
            walk(next, edges, path, done, cycles);
        }
        path.pop();
    }
    let mut cycles = BTreeSet::new();
    let mut done = BTreeSet::new();
    for start in edges.keys() {
        walk(start, edges, &mut Vec::new(), &mut done, &mut cycles);
    }
    for cycle in cycles {
        let mut shown = cycle.clone();
        shown.push(cycle[0].clone());
        out.push(Finding {
            code: "loc_fallback_cycle",
            severity: Severity::Warning,
            locale: cycle[0].clone(),
            key: None,
            file: None,
            line: None,
            message: format!("fallback edges form a cycle: {}", shown.join(" -> ")),
        });
    }
}

/// `loc_plural_missing` and `loc_plural_unreachable` for every plural selector of `parsed`.
fn plural_findings(parsed: &ParsedLocale, out: &mut Vec<Finding>) {
    let tag = parsed.locale.as_str();
    for (key, message) in &parsed.messages {
        for selector in message.selectors.iter().filter(|s| s.is_plural()) {
            // An ordinal selector picks among the ordinal categories (`[two]`, `[few]` in
            // English), a cardinal one among the cardinal categories.
            let kind = if selector.ordinal {
                plural::PluralKind::Ordinal
            } else {
                plural::PluralKind::Cardinal
            };
            let Some(categories) = plural::categories_of_kind(tag, kind) else {
                continue;
            };
            let named: BTreeSet<PluralCategory> = selector
                .keys
                .iter()
                .filter_map(|k| PluralCategory::parse(k))
                .collect();
            let default = PluralCategory::parse(&selector.default);
            let missing: Vec<&str> = categories
                .iter()
                .filter(|c| !named.contains(c) && Some(**c) != default)
                // A default that is not a category (`*[many-items]`) covers `other`.
                .filter(|c| !(**c == PluralCategory::Other && default.is_none()))
                .map(|c| c.as_str())
                .collect();
            if !missing.is_empty() {
                out.push(finding(
                    "loc_plural_missing",
                    Severity::Warning,
                    tag,
                    Some(message),
                    key,
                    format!(
                        "`{key}` has no variant for the {tag} plural categories {missing:?}; they fall to *[{}]",
                        selector.default
                    ),
                ));
            }
            for unreachable in named.iter().filter(|c| !categories.contains(c)) {
                out.push(finding(
                    "loc_plural_unreachable",
                    Severity::Warning,
                    tag,
                    Some(message),
                    key,
                    format!(
                        "`{key}` has a [{unreachable}] variant, but {tag} never selects {unreachable} (use an exact number such as [0])"
                    ),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locale(tag: &str, text: &str) -> ParsedLocale {
        parse_locale(tag, &[(format!("{tag}/main.ftl"), text.to_owned())])
    }

    /// Corr-ui-b: `NUMBER($n, type: "ordinal")` selects ordinal categories at run time
    /// (ADR-3359), but the check compared its keys with the cardinal ones, so a correct English
    /// ordinal got `loc_plural_unreachable` for `[two]` and `[few]`, and one missing them passed.
    #[test]
    fn ordinal_selectors_check_against_ordinal_categories() {
        let full = locale(
            "en",
            "place = { NUMBER($n, type: \"ordinal\") ->\n    [one] { $n }st\n    [two] { $n }nd\n    [few] { $n }rd\n   *[other] { $n }th\n}\n",
        );
        assert!(full.messages["place"].selectors[0].ordinal);
        let mut out = Vec::new();
        plural_findings(&full, &mut out);
        assert!(out.is_empty(), "{out:?}");
        let short = locale(
            "en",
            "place = { NUMBER($n, type: \"ordinal\") ->\n    [one] { $n }st\n   *[other] { $n }th\n}\n",
        );
        let mut out = Vec::new();
        plural_findings(&short, &mut out);
        assert!(
            out.iter().any(|f| f.code == "loc_plural_missing"
                && f.message.contains("\"two\"")
                && f.message.contains("\"few\"")),
            "{out:?}"
        );
        // A cardinal selector still uses the cardinal table.
        let cardinal = locale(
            "en",
            "n = { $n ->\n    [one] one\n    [two] two\n   *[other] many\n}\n",
        );
        assert!(!cardinal.messages["n"].selectors[0].ordinal);
        let mut out = Vec::new();
        plural_findings(&cardinal, &mut out);
        assert!(out.iter().any(|f| f.code == "loc_plural_unreachable"), "{out:?}");
    }

    #[test]
    fn parse_records_lines_variables_and_selectors() {
        let p = locale(
            "en",
            "# comment\nhello = Hello { $name }\n\nitems = { $count ->\n    [one] One item\n   *[other] { $count } items\n}\nref = { hello }!\n",
        );
        let hello = &p.messages["hello"];
        assert_eq!(hello.line, 2);
        assert!(hello.variables.contains("name"));
        let items = &p.messages["items"];
        assert_eq!(items.line, 4);
        assert_eq!(items.selectors.len(), 1);
        assert_eq!(items.selectors[0].variable.as_deref(), Some("count"));
        assert_eq!(items.selectors[0].default, "other");
        assert!(items.selectors[0].is_plural());
        assert!(p.messages["ref"].references.contains("hello"));
        assert!(p.findings.is_empty(), "{:?}", p.findings);
    }

    #[test]
    fn parse_errors_have_a_line() {
        let p = locale("en", "ok = fine\nbroken = { $\n");
        let errors: Vec<_> = p
            .findings
            .iter()
            .filter(|f| f.code == "loc_parse_error")
            .collect();
        assert_eq!(errors.len(), 1, "{:?}", p.findings);
        assert_eq!(errors[0].line, Some(2));
        assert!(p.messages.contains_key("ok"));
    }

    #[test]
    fn plural_checks_follow_the_locale() {
        let ru = locale(
            "ru",
            "items = { $count ->\n    [one] {$count} предмет\n   *[other] {$count} предметов\n}\n",
        );
        let en = locale(
            "en",
            "items = { $count ->\n    [one] One\n    [few] Few\n   *[other] Many\n}\n",
        );
        let report = validate(&[en, ru], &CheckOptions::default());
        let missing: Vec<_> = report.with_code("loc_plural_missing").collect();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].locale, "ru");
        assert!(missing[0].message.contains("few") && missing[0].message.contains("many"));
        let unreachable: Vec<_> = report.with_code("loc_plural_unreachable").collect();
        assert_eq!(unreachable.len(), 1);
        assert_eq!(unreachable[0].locale, "en");
    }

    #[test]
    fn duplicates_are_reported() {
        let p = locale("en", "a = one\na = two\n");
        assert_eq!(p.findings[0].code, "loc_duplicate_key");
        assert_eq!(p.findings[0].line, Some(2));
    }

    #[test]
    fn a_missing_self_name_warns_once_any_locale_has_one() {
        let en = locale("en", "locale-name = English\nhello = Hello\n");
        let de = locale("de", "hello = Hallo\n");
        let fr = locale("fr", "locale-name = Français\nhello = Salut\n");
        let report = validate(&[en, de, fr], &CheckOptions::default());
        let found: Vec<_> = report.with_code("loc_no_self_name").collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].locale, "de");
        assert_eq!(found[0].severity, Severity::Warning);
        // No locale defines one: no language menu to fill, no warning.
        let en = locale("en", "hello = Hello\n");
        let de = locale("de", "hello = Hallo\n");
        let report = validate(&[en, de], &CheckOptions::default());
        assert_eq!(report.with_code("loc_no_self_name").count(), 0);
    }

    #[test]
    fn fallback_cycles_are_reported_once_on_the_smallest_tag() {
        let mut fallbacks = BTreeMap::new();
        fallbacks.insert("pt".to_owned(), vec!["es".to_owned()]);
        fallbacks.insert("es".to_owned(), vec!["ca".to_owned()]);
        fallbacks.insert("ca".to_owned(), vec!["pt".to_owned()]);
        fallbacks.insert("de".to_owned(), vec!["de".to_owned()]);
        fallbacks.insert("fr".to_owned(), vec!["en".to_owned()]);
        let mut out = Vec::new();
        cycle_findings(&fallbacks, &mut out);
        let messages: Vec<_> = out
            .iter()
            .map(|f| (f.locale.as_str(), f.message.as_str()))
            .collect();
        assert_eq!(
            messages,
            vec![
                ("ca", "fallback edges form a cycle: ca -> pt -> es -> ca"),
                ("de", "fallback edges form a cycle: de -> de"),
            ]
        );
        assert!(
            out.iter()
                .all(|f| f.code == "loc_fallback_cycle" && f.severity == Severity::Warning)
        );
        // An acyclic graph is quiet.
        let mut acyclic = BTreeMap::new();
        acyclic.insert("a".to_owned(), vec!["b".to_owned(), "c".to_owned()]);
        acyclic.insert("b".to_owned(), vec!["c".to_owned()]);
        let mut out = Vec::new();
        cycle_findings(&acyclic, &mut out);
        assert!(out.is_empty());
    }
}
