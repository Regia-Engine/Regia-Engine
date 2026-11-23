// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask loc-length`: fail when a Fluent message grows past its character budget
//! under pseudo-localization (ADR-4564).
//!
//! The length is the number of Unicode scalar values in the string
//! [`regia_ui::loc_pseudo::pseudo_localize_ftl`] returns for the message block (the id
//! line, continuation lines and attributes; comments are not part of the block). That is
//! the same block the localization compiler stores. Placeables, variant keys and the id
//! stay as syntax, the way that function leaves them.
//!
//! Defaults match [`regia_ui::loc_pseudo::PseudoOptions`]: expansion `0.3`, accents on,
//! brackets on. `--expansion` overrides the ratio. `--no-accents` and `--no-brackets`
//! turn those off. A non-finite expansion counts as 0, and the ratio is clamped to
//! `0.0..=2.0`, both inside `pseudo_localize_ftl`.
//!
//! A message with no budget line is skipped and printed as `skipped <key>`. A budget key
//! that is not a message in the file is `loc_budget_missing` and fails the run. Terms
//! (ids that start with `-`) are not messages. Exit 0 when every budgeted key's length
//! is at most its budget. Exit 1 when one or more exceed, when a budget key is missing,
//! or when a budget line does not parse. Exceeding keys print
//! `<key> <pseudo length> <budget>`, sorted by key.
//!
//! The command reads only the two files. It does not use the clock or the network.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use regia_ui::loc_pseudo::{PseudoOptions, pseudo_localize_ftl};

/// The expansion ratio [`PseudoOptions::default`] uses.
pub const DEFAULT_EXPANSION: f32 = 0.3;

/// Printed when a budget key is not a message in the Fluent file.
pub const MISSING_CODE: &str = "loc_budget_missing";

/// `cargo xtask loc-length` arguments.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Fluent file (`.ftl`) whose messages are measured.
    #[arg(long)]
    pub ftl: PathBuf,
    /// Budget file. Each line is `key = max_chars`. Blank lines and `#` comments are ignored.
    #[arg(long)]
    pub budget: PathBuf,
    /// Extra length as a ratio of the letters in each text run.
    ///
    /// The default is `0.3`, the same default `PseudoOptions` uses.
    #[arg(long, default_value_t = DEFAULT_EXPANSION)]
    pub expansion: f32,
    /// Leave ascii letters unchanged. The default replaces them with accented look-alikes.
    #[arg(long)]
    pub no_accents: bool,
    /// Do not wrap text runs in brackets. The default wraps them.
    #[arg(long)]
    pub no_brackets: bool,
}

impl Args {
    /// The pseudo-localization settings these arguments select.
    #[must_use]
    pub fn pseudo_options(&self) -> PseudoOptions {
        pseudo_options(self.expansion, !self.no_accents, !self.no_brackets)
    }
}

/// Settings for [`check`], in the same shape `pseudo_localize_ftl` accepts.
#[must_use]
pub fn pseudo_options(expansion: f32, accents: bool, brackets: bool) -> PseudoOptions {
    PseudoOptions {
        expansion,
        accents,
        brackets,
    }
}

/// The defaults `PseudoOptions` uses: expansion [`DEFAULT_EXPANSION`], accents on, brackets on.
#[must_use]
pub fn default_pseudo_options() -> PseudoOptions {
    PseudoOptions::default()
}

/// One message whose pseudo-localized block is longer than its budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverBudget {
    /// The message id.
    pub key: String,
    /// Unicode scalar values in the pseudo-localized message block.
    pub length: usize,
    /// The budget from the budget file, in characters.
    pub budget: u64,
}

/// The result of comparing one Fluent file to one budget file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LengthReport {
    /// Budgeted messages whose pseudo-localized block is longer than the budget, sorted by key.
    pub exceeded: Vec<OverBudget>,
    /// Budget keys that are not messages in the Fluent file, sorted.
    pub missing: Vec<String>,
    /// Message ids that have no budget line, sorted. These do not fail the run.
    pub skipped: Vec<String>,
    /// Budget lines that could not be read. These fail the run.
    pub errors: Vec<String>,
}

impl LengthReport {
    /// Whether every budgeted key fits and the budget file parsed.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.exceeded.is_empty() && self.missing.is_empty() && self.errors.is_empty()
    }

    /// How many rows fail the run. Skipped keys are not counted.
    #[must_use]
    pub fn problem_count(&self) -> usize {
        self.exceeded.len() + self.missing.len() + self.errors.len()
    }
}

/// Unicode scalar values in the pseudo-localized form of one message block.
#[must_use]
pub fn pseudo_length(block: &str, options: &PseudoOptions) -> usize {
    pseudo_localize_ftl(block, options).chars().count()
}

/// Compare `ftl` to `budget_text`.
///
/// `budget_text` is `key = max_chars` lines. A length equal to the budget fits.
#[must_use]
pub fn check(ftl: &str, budget_text: &str, options: &PseudoOptions) -> LengthReport {
    let messages = message_blocks(ftl);
    let (budgets, errors) = parse_budget(budget_text);
    let mut skipped = Vec::new();
    let mut exceeded = Vec::new();
    for (key, raw) in &messages {
        match budgets.get(key) {
            None => skipped.push(key.clone()),
            Some(&budget) => {
                let length = pseudo_length(raw, options);
                let over = u64::try_from(length).map_or(true, |n| n > budget);
                if over {
                    exceeded.push(OverBudget {
                        key: key.clone(),
                        length,
                        budget,
                    });
                }
            }
        }
    }
    let missing = budgets
        .keys()
        .filter(|key| !messages.contains_key(key.as_str()))
        .cloned()
        .collect();
    LengthReport {
        exceeded,
        missing,
        skipped,
        errors,
    }
}

/// The lines the command prints for `report`, in a stable order.
///
/// Skipped keys, then `loc_budget_missing` keys, then `<key> <length> <budget>` for keys
/// that exceed, then budget parse errors. Each group is sorted by key (errors keep file order).
#[must_use]
pub fn lines(report: &LengthReport) -> Vec<String> {
    let mut out = Vec::new();
    for key in &report.skipped {
        out.push(format!("skipped {key}"));
    }
    for key in &report.missing {
        out.push(format!("{MISSING_CODE} {key}"));
    }
    for row in &report.exceeded {
        out.push(format!("{} {} {}", row.key, row.length, row.budget));
    }
    for err in &report.errors {
        out.push(format!("error: {err}"));
    }
    out
}

/// Exit 0 when [`LengthReport::ok`], otherwise exit 1.
#[must_use]
pub fn exit_code(report: &LengthReport) -> ExitCode {
    if report.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Print `report`, with a problem count on stderr.
pub fn print_report(report: &LengthReport) {
    for line in lines(report) {
        println!("{line}");
    }
    let problems = report.problem_count();
    if problems == 0 {
        eprintln!("loc-length: ok");
    } else {
        eprintln!("loc-length: {problems} problem(s)");
    }
}

/// Read the two files, compare them, print the report.
///
/// # Errors
///
/// When either file cannot be read.
pub fn run(args: &Args) -> Result<ExitCode> {
    let ftl =
        std::fs::read_to_string(&args.ftl).with_context(|| format!("reading {}", args.ftl.display()))?;
    let budget = std::fs::read_to_string(&args.budget)
        .with_context(|| format!("reading {}", args.budget.display()))?;
    let report = check(&ftl, &budget, &args.pseudo_options());
    print_report(&report);
    Ok(exit_code(&report))
}

fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Message id to its raw block. The last definition of a key wins, as the compiler does.
fn message_blocks(ftl: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for block in regia_ui::loc_po::ftl_blocks(strip_bom(ftl)) {
        out.insert(block.key, block.raw);
    }
    out
}

fn parse_budget(text: &str) -> (BTreeMap<String, u64>, Vec<String>) {
    let mut map = BTreeMap::new();
    let mut errors = Vec::new();
    for (index, raw) in strip_bom(text).lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line_no = index + 1;
        let Some((key, value)) = line.split_once('=') else {
            errors.push(format!("bad budget line {line_no}: missing '='"));
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || key.chars().any(char::is_whitespace) {
            errors.push(format!(
                "bad budget line {line_no}: key {key:?} is empty or contains spaces"
            ));
            continue;
        }
        let Ok(budget) = value.parse::<u64>() else {
            errors.push(format!(
                "bad budget line {line_no}: {value:?} is not a non-negative integer"
            ));
            continue;
        };
        if map.contains_key(key) {
            errors.push(format!("bad budget line {line_no}: duplicate key {key}"));
            continue;
        }
        map.insert(key.to_owned(), budget);
    }
    (map, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(clap::Parser)]
    struct TestCli {
        #[command(flatten)]
        args: Args,
    }

    const FTL: &str = "\
short = Hi
long = This label is deliberately long enough to fail a small budget
note = Just a note
";

    const BUDGET: &str = "\
# characters after pseudo-localization
short = 80
long = 12
";

    fn fixture(name: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/loc_length")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
    }

    #[test]
    fn cli_defaults_match_pseudo_options() {
        let defaults = PseudoOptions::default();
        assert_eq!(defaults.expansion, DEFAULT_EXPANSION);
        assert!(defaults.accents);
        assert!(defaults.brackets);
        assert_eq!(default_pseudo_options(), defaults);
        assert_eq!(pseudo_options(DEFAULT_EXPANSION, true, true), defaults);
        let cli = <TestCli as clap::Parser>::try_parse_from([
            "loc-length",
            "--ftl",
            "messages.ftl",
            "--budget",
            "budget.txt",
        ])
        .expect("default arguments parse");
        assert_eq!(cli.args.pseudo_options(), defaults);
    }

    #[test]
    fn cli_flags_control_the_measured_length() {
        let cli = <TestCli as clap::Parser>::try_parse_from([
            "loc-length",
            "--ftl",
            "messages.ftl",
            "--budget",
            "budget.txt",
            "--expansion",
            "0",
            "--no-accents",
            "--no-brackets",
        ])
        .expect("custom arguments parse");
        let report = check("short = Hi\n", "short = 10\n", &cli.args.pseudo_options());
        assert!(report.ok(), "{report:?}");
        let expanded = check("short = Hi\n", "short = 10\n", &PseudoOptions::default());
        assert_eq!(expanded.exceeded.len(), 1);
    }

    #[test]
    fn unicode_scalars_count_once_regardless_of_utf8_width() {
        // Five CJK/emoji scalars occupy more than five UTF-8 bytes.
        let ftl = "title = 日本語😀é\n";
        let options = pseudo_options(0.0, false, false);
        let report = check(ftl, "title = 13\n", &options);
        assert!(report.ok(), "{report:?}");
        let report = check(ftl, "title = 12\n", &options);
        assert_eq!(
            report.exceeded,
            [OverBudget {
                key: "title".into(),
                length: 13,
                budget: 12,
            }]
        );
    }

    #[test]
    fn fixture_has_one_key_under_budget_and_one_over() {
        let report = check(
            &fixture("messages.ftl"),
            &fixture("budget.txt"),
            &default_pseudo_options(),
        );
        assert_eq!(
            report
                .exceeded
                .iter()
                .map(|row| row.key.as_str())
                .collect::<Vec<_>>(),
            ["long"]
        );
        let over = &report.exceeded[0];
        assert!(over.length > usize::try_from(over.budget).expect("budget"));
        assert_eq!(over.budget, 12);
        let raw = message_blocks(&fixture("messages.ftl"));
        assert_eq!(
            over.length,
            pseudo_length(&raw["long"], &default_pseudo_options())
        );
        assert!(!report.exceeded.iter().any(|row| row.key == "short"));
        assert_eq!(report.skipped, ["note"]);
        assert!(report.missing.is_empty());
        assert!(report.errors.is_empty());
        assert!(!report.ok());
        assert_eq!(exit_code(&report), ExitCode::FAILURE);
        let printed = lines(&report);
        assert_eq!(printed[0], "skipped note");
        assert_eq!(printed[1], format!("long {} 12", over.length));
    }

    #[test]
    fn a_missing_budget_key_is_loc_budget_missing_and_an_unbudgeted_key_is_skipped() {
        let ftl = "alpha = Hi\nzeta = Hello\n";
        let budget = "alpha = 1000\nmissing-key = 4\n";
        let report = check(ftl, budget, &default_pseudo_options());
        assert!(report.exceeded.is_empty());
        assert_eq!(report.skipped, ["zeta"]);
        assert_eq!(report.missing, ["missing-key"]);
        assert_eq!(lines(&report), ["skipped zeta", "loc_budget_missing missing-key"]);
        assert_eq!(exit_code(&report), ExitCode::FAILURE);
    }

    #[test]
    fn exceeding_keys_print_sorted_by_key() {
        let ftl = "zeta = This one is long\nalpha = This one is also long\n";
        let budget = "zeta = 1\nalpha = 1\n";
        let report = check(ftl, budget, &default_pseudo_options());
        let keys: Vec<_> = report.exceeded.iter().map(|row| row.key.as_str()).collect();
        assert_eq!(keys, ["alpha", "zeta"]);
        let printed: Vec<_> = lines(&report)
            .into_iter()
            .filter(|line| !line.starts_with("skipped") && !line.starts_with("error:"))
            .collect();
        assert_eq!(printed.len(), 2);
        assert!(printed[0].starts_with("alpha "));
        assert!(printed[1].starts_with("zeta "));
        for (line, row) in printed.iter().zip(&report.exceeded) {
            assert_eq!(line, &format!("{} {} {}", row.key, row.length, row.budget));
        }
    }

    #[test]
    fn a_length_equal_to_the_budget_fits() {
        let ftl = "short = Hi\n";
        let options = default_pseudo_options();
        let length = pseudo_length("short = Hi", &options);
        let exact = check(ftl, &format!("short = {length}\n"), &options);
        assert!(exact.ok(), "{exact:?}");
        assert_eq!(exit_code(&exact), ExitCode::SUCCESS);
        let under = check(ftl, &format!("short = {}\n", length - 1), &options);
        assert_eq!(under.exceeded.len(), 1);
        assert_eq!(under.exceeded[0].length, length);
        assert_eq!(exit_code(&under), ExitCode::FAILURE);
    }

    #[test]
    fn the_measured_length_is_the_pseudo_localized_block() {
        let block = "greet = Hello { $name }\n    .title = Tip";
        let options = default_pseudo_options();
        assert_eq!(
            pseudo_length(block, &options),
            pseudo_localize_ftl(block, &options).chars().count()
        );
        let plain = pseudo_options(0.0, false, false);
        assert_eq!(pseudo_length(block, &plain), block.chars().count());
        assert!(pseudo_length(block, &options) > pseudo_length(block, &plain));
    }

    #[test]
    fn the_string_fixture_matches_the_files() {
        let report = check(FTL, BUDGET, &default_pseudo_options());
        let from_files = check(
            &fixture("messages.ftl"),
            &fixture("budget.txt"),
            &default_pseudo_options(),
        );
        assert_eq!(report, from_files);
    }

    #[test]
    fn comments_blank_lines_crlf_and_a_bom_are_ignored() {
        let ftl = "\u{feff}short = Hi\r\n\r\n# comment\r\nlong = Way too long for four\r\n";
        let budget = "\u{feff}# header\r\n\r\nshort = 80\r\nlong = 4\r\n";
        let report = check(ftl, budget, &default_pseudo_options());
        assert_eq!(report.exceeded.len(), 1);
        assert_eq!(report.exceeded[0].key, "long");
        assert!(report.errors.is_empty());
        assert!(report.missing.is_empty());
    }

    #[test]
    fn a_bad_budget_line_and_a_duplicate_key_fail() {
        let ftl = "short = Hi\n";
        let budget = "short = 1000\nshort = 1\nnot a line\nwide = no\n";
        let report = check(ftl, budget, &default_pseudo_options());
        assert!(!report.ok());
        assert_eq!(report.errors.len(), 3);
        assert!(report.errors.iter().any(|e| e.contains("duplicate key short")));
        assert!(report.errors.iter().any(|e| e.contains("missing '='")));
        assert!(report.errors.iter().any(|e| e.contains("non-negative")));
        assert!(report.exceeded.is_empty(), "the first budget is kept");
    }

    #[test]
    fn the_later_message_wins_and_a_term_is_not_a_message() {
        let ftl = "short = This first copy is very long indeed\n-brand = Acme\nshort = Hi\n";
        let options = default_pseudo_options();
        let length = pseudo_length("short = Hi", &options);
        let report = check(ftl, &format!("short = {length}\n-brand = 1\n"), &options);
        assert!(report.exceeded.is_empty(), "{report:?}");
        assert_eq!(report.missing, ["-brand"]);
        assert_eq!(exit_code(&report), ExitCode::FAILURE);
    }

    #[test]
    fn an_empty_pair_fits_and_only_skipped_keys_still_fit() {
        let empty = check("", "", &default_pseudo_options());
        assert!(empty.ok());
        assert_eq!(exit_code(&empty), ExitCode::SUCCESS);
        let skipped = check("note = Hello\n", "", &default_pseudo_options());
        assert!(skipped.ok());
        assert_eq!(skipped.skipped, ["note"]);
        assert_eq!(lines(&skipped), ["skipped note"]);
    }
}
