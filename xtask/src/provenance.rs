// SPDX-License-Identifier: MPL-2.0
//! Row-level source declarations for `docs/provenance.md` (cleanroom § 2, ADR-4581).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;

/// A source-record problem, with a one-based Markdown line number.
#[derive(Debug, PartialEq, Eq)]
pub struct Finding {
    /// Stable lowercase diagnostic code.
    pub code: &'static str,
    /// One-based line in `docs/provenance.md`.
    pub line: usize,
    /// Actionable explanation.
    pub message: String,
}

/// Counts and findings from a provenance log.
#[derive(Debug, Default)]
pub struct Report {
    /// Number of data rows, including invalid rows.
    pub rows: usize,
    /// Rows by their declared primary source class.
    pub classes: BTreeMap<String, usize>,
    /// Record defects; an empty list is a passing check.
    pub findings: Vec<Finding>,
}

const CLASSES: &[&str] = &[
    "public-literature",
    "public-documentation",
    "standard",
    "permissive-code",
    "reader-builder",
    "regia-design",
    "licensed-vendor-sdk",
    "do27-owner-code",
];

/// The owner's copy exception for Direct Order 27 code (ADR-8012 part A-1). A `do27-owner-code`
/// row must name it as `; approval: ADR-8012`.
const DO27_APPROVAL: &str = "ADR-8012";

/// Owner approvals of a licensed vendor SDK (ADR-4931): the approval ADR, the license name and
/// the license's public URL. A `licensed-vendor-sdk` row must name one approval here exactly,
/// and its evidence must name that license and URL. A new SDK needs its own owner ADR first.
const VENDOR_SDK_APPROVALS: &[(&str, &str, &str)] = &[(
    "ADR-2002",
    "NVIDIA RTX SDKs license",
    "https://github.com/NVIDIA/DLSS/blob/v310.9.1/LICENSE.txt",
)];

/// SPDX license exceptions that only add permissions to an allowed license (ADR-4931), with
/// the one license each may follow.
const LICENSE_EXCEPTIONS: &[(&str, &str)] = &[("Apache-2.0", "LLVM-exception")];

const LICENSES: &[&str] = &[
    "MIT",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "Zlib",
    "ISC",
    "CC0-1.0",
    "0BSD",
    "Unlicense",
    "BSL-1.0",
];

impl Report {
    fn problem(&mut self, line: usize, code: &'static str, message: &str) {
        self.findings.push(Finding {
            code,
            line,
            message: message.into(),
        });
    }
}

// Markdown table pipes must be escaped even inside a code span. An even run of backslashes
// does not escape a pipe. Reject malformed rows instead of guessing which cell is the source.
fn cells(line: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut slashes = 0usize;
    for (index, c) in line.char_indices() {
        if c == '|' && slashes.is_multiple_of(2) {
            result.push(line[start..index].trim());
            start = index + 1;
        }
        slashes = if c == '\\' { slashes + 1 } else { 0 };
    }
    result.push(line[start..].trim());
    if result.first() == Some(&"") {
        result.remove(0);
    }
    if result.last() == Some(&"") {
        result.pop();
    }
    result
}

fn public_page(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !host.contains('.')
        || host.starts_with('.')
        || host.ends_with('.')
        || host.contains('@')
        || host.contains(':')
    {
        return false;
    }
    if host.parse::<std::net::Ipv4Addr>().is_ok()
        || [".localhost", ".local", ".internal", ".invalid"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
    {
        return false;
    }
    let lower = rest.to_ascii_lowercase();
    if [
        "github.com",
        "raw.githubusercontent.com",
        "gitlab.com",
        "bitbucket.org",
        "codeberg.org",
        "git.sr.ht",
        "sourceforge.net",
    ]
    .contains(&host.as_str())
        || host.starts_with("raw.")
        || ["/blob/", "/raw/", "/decompiled/", "/source-repository/"]
            .iter()
            .any(|bad| lower.contains(bad))
    {
        return false;
    }
    let path = lower.split(['?', '#']).next().unwrap_or_default();
    ![
        ".h", ".hpp", ".hxx", ".inl", ".inc", ".c", ".cc", ".cpp", ".rs", ".cs", ".lua", ".py", ".js",
        ".usf", ".ush", ".wgsl", ".wesl", ".hlsl", ".hlsli", ".glsl", ".vert", ".frag", ".metal", ".spv",
        ".git",
    ]
    .iter()
    .any(|suffix| path.ends_with(suffix))
}

fn has_public_page(source: &str) -> bool {
    source
        .split(|c: char| c.is_whitespace() || "<>[]()\"`".contains(c))
        .any(|part| public_page(part.trim_end_matches([',', ';', '.'])))
}

fn mentions_set(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["public-sources", "set area", "set spec", "set ids", "adr-1526"]
        .iter()
        .any(|phrase| lower.contains(phrase))
}

fn has_spec_link(source: &str) -> bool {
    source.split("](").skip(1).any(|rest| {
        rest.split(')').next().is_some_and(|target| {
            let path = target.split('#').next().unwrap_or_default();
            path.ends_with(".md") && !path.starts_with("file:") && !path.is_empty()
        })
    })
}

fn allowed_license(declaration: &str) -> bool {
    let Some((_, expression)) = declaration.split_once("; license: ") else {
        return false;
    };
    // license [WITH exception] { (OR | AND) license [WITH exception] }
    let mut tokens = expression.split_whitespace().peekable();
    loop {
        let Some(license) = tokens.next() else {
            return false;
        };
        if !LICENSES.contains(&license) {
            return false;
        }
        if tokens.next_if_eq(&"WITH").is_some() {
            let Some(exception) = tokens.next() else {
                return false;
            };
            if !LICENSE_EXCEPTIONS.contains(&(license, exception)) {
                return false;
            }
        }
        match tokens.next() {
            None => return true,
            Some("OR" | "AND") => {}
            Some(_) => return false,
        }
    }
}

/// The owner approval a `licensed-vendor-sdk` declaration names, if it is a recorded one.
fn vendor_sdk_approval(declaration: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    let adr = declaration
        .split(';')
        .skip(1)
        .find_map(|term| term.trim().strip_prefix("approval: "))?;
    VENDOR_SDK_APPROVALS
        .iter()
        .find(|(approval, _, _)| *approval == adr.trim())
}

/// A DO27 commit in the evidence: a 9 to 40 digit lowercase hex token with at least one digit.
fn has_commit_sha(evidence: &str) -> bool {
    evidence.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| {
        (9..=40).contains(&t.len())
            && t.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && t.bytes().any(|b| b.is_ascii_digit())
    })
}

/// Check the declarations in a Markdown provenance log without opening its cited sources.
///
/// Each data row needs four cells and a `[class: …]` prefix in its source cell. This is a
/// completeness check of recorded evidence, not a certification of an author's reading history.
pub fn check_text(text: &str) -> Report {
    let mut report = Report::default();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let row = cells(line);
        if !row.is_empty()
            && row
                .iter()
                .all(|cell| !cell.is_empty() && cell.chars().all(|c| "-: ".contains(c)))
        {
            continue;
        }
        if row.len() == 4 && row[0] == "Area" && row[1] == "What" && row[2].starts_with("Source") {
            continue;
        }
        let number = index + 1;
        report.rows += 1;
        if row.len() != 4 || !line.ends_with('|') {
            report.problem(
                number,
                "row_shape",
                "expected four cells; escape literal pipes and retain a Notes cell",
            );
            continue;
        }
        let source = row[2];
        let declaration = source
            .strip_prefix("[class: ")
            .and_then(|rest| rest.split_once(']'));
        let Some((declaration, evidence)) = declaration else {
            report.problem(
                number,
                "source_class",
                "declare a cleanroom source class in the Source cell",
            );
            continue;
        };
        let class = declaration.split(';').next().unwrap_or_default();
        if !CLASSES.contains(&class) {
            report.problem(
                number,
                "source_class",
                "source class is not allowed by cleanroom section 2",
            );
            continue;
        }
        *report.classes.entry(class.into()).or_default() += 1;
        if evidence.trim().is_empty()
            || ["todo", "tbd", "unknown", "none"].contains(&evidence.trim().to_ascii_lowercase().as_str())
        {
            report.problem(
                number,
                "source_missing",
                "retain a substantive citation or describe Regia's own design",
            );
        }
        if class == "permissive-code" && !allowed_license(declaration) {
            report.problem(
                number,
                "code_license",
                "declare an allowed permissive SPDX license using '; license: …'",
            );
        }
        if class == "licensed-vendor-sdk" {
            match vendor_sdk_approval(declaration) {
                None => report.problem(
                    number,
                    "sdk_approval",
                    "declare the owner's license approval ADR for this SDK using '; approval: ADR-…'",
                ),
                Some((_, license, url)) => {
                    if !evidence.contains(license) || !evidence.contains(url) {
                        report.problem(
                            number,
                            "sdk_license",
                            "name the approved SDK license and its public license URL in the Source cell",
                        );
                    }
                }
            }
        }
        if class == "do27-owner-code" {
            let approved = declaration
                .split(';')
                .skip(1)
                .any(|term| term.trim().strip_prefix("approval: ") == Some(DO27_APPROVAL));
            if !approved {
                report.problem(
                    number,
                    "do27_approval",
                    "cite the owner's DO27 copy exception using '; approval: ADR-8012'",
                );
            }
            let marked = ["as-is", "adapt", "requirements"]
                .iter()
                .any(|mark| evidence.contains(mark));
            if !has_commit_sha(evidence) || !marked {
                report.problem(
                    number,
                    "do27_source",
                    "name the DO27 crate, file, commit SHA and mark (as-is, adapt or requirements); the file also needs a [[do27]] row in docs/ported-sources.toml",
                );
            }
        }
        if mentions_set(line) && !has_public_page(source) {
            report.problem(
                number,
                "set_url_missing",
                "set-derived records need a public-page http(s) URL in the Source cell",
            );
        }
        if class == "reader-builder" {
            let process = source.to_ascii_lowercase();
            if !process.contains("adr-1441") {
                report.problem(
                    number,
                    "reader_authorization",
                    "cite the narrow owner authorization ADR-1441 for a reader/builder source",
                );
            }

            if ![
                "reader wrote a behavior-only spec",
                "builder worked from the spec only",
                "never saw the gpl source",
            ]
            .iter()
            .all(|phrase| process.contains(phrase))
            {
                report.problem(number, "reader_process", "record that the reader wrote a behavior-only spec and the builder worked from that spec only and never saw the GPL source");
            }
            if !has_spec_link(source) {
                report.problem(number, "reader_spec_link", "link the retained behavior spec");
            }
        }
    }
    if report.rows == 0 {
        report.problem(1, "no_rows", "no provenance data rows found");
    }
    report
}

/// Read the repository's provenance log and check its source declarations.
pub fn check(root: &Path) -> std::io::Result<Report> {
    Ok(check_text(&std::fs::read_to_string(
        root.join("docs/provenance.md"),
    )?))
}

/// Print row counts and diagnostics; fail if a provenance record is incomplete.
pub fn run(root: &Path) -> std::io::Result<ExitCode> {
    let report = check(root)?;
    println!("check-provenance: {} rows", report.rows);
    for (class, count) in report.classes {
        println!("  {class}: {count}");
    }
    for finding in &report.findings {
        eprintln!(
            "docs/provenance.md:{}: [{}] {}",
            finding.line, finding.code, finding.message
        );
    }
    Ok(if report.findings.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(source: &str, notes: &str) -> String {
        format!(
            "# Provenance\n\n| Area | What | Source | Notes |\n|---|---|---|---|\n| Fixture | Algorithm | {source} | {notes} |\n"
        )
    }

    fn codes(source: &str, notes: &str) -> Vec<&'static str> {
        check_text(&log(source, notes))
            .findings
            .into_iter()
            .map(|f| f.code)
            .collect()
    }

    #[test]
    fn each_allowed_source_class_has_a_concrete_record() {
        for source in [
            "[class: public-literature] Author, Published article, 2020",
            "[class: public-documentation] Public manual, https://example.org/manual",
            "[class: standard] RFC 9110 section 14",
            "[class: permissive-code; license: MIT OR Apache-2.0] Project, https://example.org/repo",
            "[class: regia-design] Original design: fixed-step work queue",
        ] {
            let report = check_text(&log(source, "Original implementation"));
            assert!(report.findings.is_empty(), "{source}: {:?}", report.findings);
            assert_eq!(report.rows, 1);
            assert_eq!(report.classes.values().sum::<usize>(), 1);
        }
    }

    #[test]
    fn do27_owner_code_rows_need_the_owner_exception_sha_and_mark() {
        let good = "[class: do27-owner-code; approval: ADR-8012] Direct Order 27 `ir-sim/src/math.rs` all, commit f7e8f4740, as-is";
        let report = check_text(&log(good, "Copied under the owner's exception"));
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert_eq!(report.classes.get("do27-owner-code"), Some(&1));
        assert!(
            codes(
                "[class: do27-owner-code] Direct Order 27 `ir-sim/src/math.rs`, commit f7e8f4740, as-is",
                ""
            )
            .contains(&"do27_approval")
        );
        assert!(
            codes("[class: do27-owner-code; approval: ADR-2002] Direct Order 27 `ir-sim/src/math.rs`, commit f7e8f4740, adapt", "")
                .contains(&"do27_approval")
        );
        assert!(
            codes(
                "[class: do27-owner-code; approval: ADR-8012] Direct Order 27 `ir-sim/src/math.rs`, as-is",
                ""
            )
            .contains(&"do27_source"),
            "a row without the commit fails"
        );
        assert!(
            codes("[class: do27-owner-code; approval: ADR-8012] Direct Order 27 `ir-sim/src/math.rs`, commit f7e8f4740", "")
                .contains(&"do27_source"),
            "a row without the mark fails"
        );
        assert!(
            codes("[class: regia-design; approval: ADR-8012] Original design", "").is_empty(),
            "the approval term opens nothing for other classes"
        );
    }

    #[test]
    fn public_documentation_hosts_are_not_confused_with_repository_hosts() {
        for url in [
            "https://github.github.com/gfm/",
            "https://example.github.io/manual/",
            "https://docs.example.org/api/Type.html",
        ] {
            let source = format!("[class: public-documentation] Set area `fixture`; <{url}>");
            assert!(codes(&source, "").is_empty(), "{url}");
        }
    }

    #[test]
    fn missing_unknown_and_empty_source_declarations_fail() {
        assert!(codes("A remembered implementation", "").contains(&"source_class"));
        assert!(codes("[class: proprietary-source] Internal checkout", "").contains(&"source_class"));
        assert!(codes("[class: public-documentation]", "").contains(&"source_missing"));
        assert!(codes("[class: public-documentation] TODO", "").contains(&"source_missing"));
    }

    #[test]
    fn a_set_area_needs_a_public_page_url_in_the_source_cell() {
        let source = "[class: public-documentation] Public-sources spec set area `area-one` (ADR-1526)";
        assert!(codes(source, "").contains(&"set_url_missing"));
        assert!(codes(source, "https://example.org/unrelated").contains(&"set_url_missing"));
        assert!(codes(&format!("{source}; https://example.org/public/manual"), "").is_empty());
        assert!(
            codes("[class: regia-design] Regia spec (set specs `area-one`)", "").contains(&"set_url_missing")
        );
    }

    #[test]
    fn repositories_and_source_files_do_not_close_a_set_url_gap() {
        for url in [
            "https://github.com/example/project",
            "https://raw.githubusercontent.com/example/project/main/file.h",
            "https://example.org/repo/blob/main/file.rs",
            "https://example.org/internal/header.hpp?download=1",
            "https://example.org/effect.usf",
            "https://example.org/effect.ush",
            "https://example.org/effect.wgsl",
            "https://example.org/effect.hlsli",
            "https://example.org/effect.wesl",
            "https://example.org/effect.vert",
            "https://example.org/effect.frag",
            "https://example.org/effect.metal",
            "https://example.org/effect.spv",
            "https://example.org/gameplay.cs",
            "https://example.org/header.inl",
            "file:///a/checkout",
            "https://example.org/decompiled/module",
            "https://codeberg.org/example/project",
            "https://example.org/project.git",
            "https://example.org/Header.HPP?download=1",
            "http://127.0.0.1/manual",
            "http://192.168.1.10/manual",
            "http://docs.local/manual",
            "https://docs.internal/manual",
        ] {
            let source = format!("[class: public-documentation] Set area `area-one` (ADR-1526); {url}");
            assert!(codes(&source, "").contains(&"set_url_missing"), "{url}");
        }
    }

    #[test]
    fn permissive_code_requires_an_allowed_license() {
        for declaration in [
            "[class: permissive-code] Project",
            "[class: permissive-code; license: GPL-3.0] Project",
            "[class: permissive-code; license: MPL-2.0] Project",
            "[class: permissive-code; license: unknown] Project",
        ] {
            assert!(codes(declaration, "").contains(&"code_license"), "{declaration}");
        }
    }

    #[test]
    fn reader_builder_records_need_process_and_a_spec_link() {
        let prefix = "[class: reader-builder] ADR-1441; reader wrote a behavior-only spec; builder worked from the spec only and never saw the GPL source";
        assert!(codes(prefix, "").contains(&"reader_spec_link"));
        assert!(
            codes("[class: reader-builder] [Behavior spec](specs/effect.md)", "").contains(&"reader_process")
        );
        assert!(codes(&format!("{prefix}; [Behavior spec](specs/effect.md)"), "").is_empty());
    }

    #[test]
    fn reader_builder_is_limited_to_the_recorded_owner_authorization() {
        let source = "[class: reader-builder] Reader wrote a behavior-only spec; builder worked from the spec only and never saw the GPL source; [Behavior spec](specs/effect.md)";
        assert!(codes(source, "").contains(&"reader_authorization"));
        assert!(codes(&format!("{source}; ADR-1441"), "").is_empty());
    }

    const SDK_SOURCE: &str = "[class: licensed-vendor-sdk; approval: ADR-2002] NVIDIA RTX SDKs license, 14 March 2024 (https://github.com/NVIDIA/DLSS/blob/v310.9.1/LICENSE.txt); programming guide sections 3.4 and 3.6";

    #[test]
    fn licensed_vendor_sdk_with_its_owner_approval_passes_and_is_counted() {
        let report = check_text(&log(SDK_SOURCE, "Original code"));
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert_eq!(report.classes.get("licensed-vendor-sdk"), Some(&1));
    }

    #[test]
    fn licensed_vendor_sdk_without_an_owner_approval_adr_fails() {
        for source in [
            "[class: licensed-vendor-sdk] NVIDIA RTX SDKs license (https://github.com/NVIDIA/DLSS/blob/v310.9.1/LICENSE.txt)",
            "[class: licensed-vendor-sdk; approval: ] NVIDIA RTX SDKs license (https://github.com/NVIDIA/DLSS/blob/v310.9.1/LICENSE.txt)",
            // An ADR that records no owner approval of this SDK license is not an approval.
            "[class: licensed-vendor-sdk; approval: ADR-9999] NVIDIA RTX SDKs license (https://github.com/NVIDIA/DLSS/blob/v310.9.1/LICENSE.txt)",
            "[class: licensed-vendor-sdk; approval: ADR-20021] NVIDIA RTX SDKs license (https://github.com/NVIDIA/DLSS/blob/v310.9.1/LICENSE.txt)",
            // Mentioning the ADR in the evidence is not the declaration.
            "[class: licensed-vendor-sdk] Approved in ADR-2002; NVIDIA RTX SDKs license (https://github.com/NVIDIA/DLSS/blob/v310.9.1/LICENSE.txt)",
        ] {
            assert!(codes(source, "").contains(&"sdk_approval"), "{source}");
        }
    }

    #[test]
    fn licensed_vendor_sdk_must_name_the_approved_license_and_its_public_url() {
        let no_url =
            "[class: licensed-vendor-sdk; approval: ADR-2002] NVIDIA RTX SDKs license, 14 March 2024";
        assert!(codes(no_url, "").contains(&"sdk_license"));
        let no_name = "[class: licensed-vendor-sdk; approval: ADR-2002] SDK guide (https://github.com/NVIDIA/DLSS/blob/v310.9.1/LICENSE.txt)";
        assert!(codes(no_name, "").contains(&"sdk_license"));
        // The license evidence in Notes does not stand in for the Source cell.
        assert!(
            codes(no_url, "https://github.com/NVIDIA/DLSS/blob/v310.9.1/LICENSE.txt")
                .contains(&"sdk_license")
        );
    }

    #[test]
    fn an_approval_term_does_not_open_other_classes_to_vendor_sdks() {
        // The SDK class is not an alias: permissive-code still needs a permissive license.
        let source = "[class: permissive-code; approval: ADR-2002] NVIDIA RTX SDKs license (https://github.com/NVIDIA/DLSS/blob/v310.9.1/LICENSE.txt)";
        assert!(codes(source, "").contains(&"code_license"));
        let proprietary = "[class: permissive-code; license: LicenseRef-NvidiaProprietary] SDK header";
        assert!(codes(proprietary, "").contains(&"code_license"));
    }

    #[test]
    fn the_llvm_exception_is_the_only_accepted_license_exception() {
        assert!(
            codes(
                "[class: permissive-code; license: Apache-2.0 WITH LLVM-exception] LLVM BinaryFormat",
                ""
            )
            .is_empty()
        );
        assert!(
            codes(
                "[class: permissive-code; license: MIT OR Apache-2.0 WITH LLVM-exception] Project",
                ""
            )
            .is_empty()
        );
        for declaration in [
            "[class: permissive-code; license: MIT WITH LLVM-exception] Project",
            "[class: permissive-code; license: Apache-2.0 WITH Classpath-exception-2.0] Project",
            "[class: permissive-code; license: GPL-2.0 WITH LLVM-exception] Project",
            "[class: permissive-code; license: Apache-2.0 WITH] Project",
            "[class: permissive-code; license: WITH LLVM-exception] Project",
            "[class: permissive-code; license: Apache-2.0 WITH LLVM-exception WITH LLVM-exception] Project",
            "[class: permissive-code; license: APSL-2.0] Project",
        ] {
            assert!(codes(declaration, "").contains(&"code_license"), "{declaration}");
        }
    }

    #[test]
    fn malformed_and_empty_logs_cannot_silently_skip_rows() {
        assert!(
            check_text("# Empty\n")
                .findings
                .iter()
                .any(|f| f.code == "no_rows")
        );
        assert!(
            check_text("| Area | What |\n")
                .findings
                .iter()
                .any(|f| f.code == "row_shape")
        );
        assert!(
            check_text("| A | B | C | D | E |\n")
                .findings
                .iter()
                .any(|f| f.code == "row_shape")
        );
        assert!(
            check_text("Fixture | Algorithm | Source | Notes\n")
                .findings
                .iter()
                .any(|f| f.code == "no_rows")
        );
    }

    #[test]
    fn escaped_pipes_and_crlf_preserve_the_four_cells() {
        let text = log(
            "[class: standard] Public standard with `A\\|B`",
            "Formula \\|x\\|",
        )
        .replace('\n', "\r\n");
        let report = check_text(&text);
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert_eq!(report.rows, 1);
    }

    #[test]
    fn diagnostics_identify_the_exact_markdown_line() {
        let report = check_text(&log("Unclassified source", ""));
        assert_eq!(report.findings[0].line, 5);
    }
}
