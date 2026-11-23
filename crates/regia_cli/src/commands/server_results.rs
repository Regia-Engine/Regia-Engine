// SPDX-License-Identifier: MPL-2.0
//! `regia server results [--since YYYY-MM-DD] [--limit N] [--dir DIR] [--json]`: the finished-match
//! records a host wrote (`regia_framework::match_results`, `book/agents/match-results.md`), newest
//! first. Without `--dir` it reads the dev server's folder, `build/server-dev/matches` under
//! `--project`. Only complete files are listed; a file that does not read is counted as skipped.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use regia_framework::match_results::{ResultPage, date_days, list_results};

/// Most records one listing prints.
pub const MAX_LIMIT: usize = 1000;

/// A server tool.
#[derive(Debug, Subcommand)]
pub enum ServerAction {
    /// List the finished-match records a host wrote, newest first.
    Results(ResultsArgs),
}

/// `regia server results`.
#[derive(Debug, Args)]
pub struct ResultsArgs {
    /// Only records dated this UTC day (`YYYY-MM-DD`) or later.
    #[arg(long)]
    pub since: Option<String>,
    /// Most records to print (1..=1000).
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// The records folder (the host's `MatchResultsSettings.root`). Default:
    /// `<project>/build/server-dev/matches`.
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Print one JSON document instead of lines.
    #[arg(long)]
    pub json: bool,
}

/// Runs a server tool.
///
/// # Errors
/// A bad `--since` or `--limit`, or a folder that cannot be read.
pub fn run(project: &Path, action: &ServerAction) -> Result<()> {
    match action {
        ServerAction::Results(args) => {
            let dir = args
                .dir
                .clone()
                .unwrap_or_else(|| crate::commands::server::dev_server_dir(project).join("matches"));
            let page = page(&dir, args.since.as_deref(), args.limit)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&page)?);
            } else {
                for line in lines(&dir, &page) {
                    println!("{line}");
                }
            }
            Ok(())
        }
    }
}

/// The records of `dir`, newest first. A folder that does not exist lists nothing.
///
/// # Errors
/// A `since` that is not a date, a `limit` outside 1..=[`MAX_LIMIT`], or an unreadable folder.
pub fn page(dir: &Path, since: Option<&str>, limit: usize) -> Result<ResultPage> {
    if !(1..=MAX_LIMIT).contains(&limit) {
        bail!("--limit {limit}: use 1..={MAX_LIMIT}");
    }
    let since = match since {
        None => None,
        Some(text) => match date_days(text) {
            Some(days) => Some(days),
            None => bail!("--since {text:?}: use a UTC date YYYY-MM-DD"),
        },
    };
    if !dir.exists() {
        return Ok(ResultPage {
            results: Vec::new(),
            total: 0,
            skipped: 0,
        });
    }
    Ok(list_results(dir, since, limit)?)
}

/// One line per record, then a summary line.
pub fn lines(dir: &Path, page: &ResultPage) -> Vec<String> {
    let mut out: Vec<String> = page
        .results
        .iter()
        .map(|r| {
            let outcome = if r.draw {
                "draw".to_owned()
            } else {
                format!("team {} won", r.winner)
            };
            format!(
                "{} {}  {} on {}  {} ({})  step {}  {} players",
                r.date,
                r.match_id,
                r.ruleset,
                if r.map_id.is_empty() { "-" } else { &r.map_id },
                outcome,
                r.end_reason.as_str(),
                r.end.step,
                r.players
            )
        })
        .collect();
    out.push(format!(
        "{} of {} record(s) in {}{}",
        page.results.len(),
        page.total,
        dir.display(),
        if page.skipped > 0 {
            format!(", {} skipped (not readable)", page.skipped)
        } else {
            String::new()
        }
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::id::Guid;
    use regia_framework::match_results::{MatchResult, write_result};

    #[test]
    fn server_results_lists_newest_first_and_counts_skipped_files() {
        let dir = regia_test_utils::TempDir::new("cli-server-results");
        for day in 1..=3u128 {
            let result = MatchResult {
                match_id: Guid::from_u128(day),
                ..MatchResult::example()
            };
            write_result(&dir, &format!("2026-11-0{day}"), &result, 0, 0).expect("write");
        }
        let torn = dir
            .join("2026-11-03")
            .join(format!("{}.json", Guid::from_u128(9)));
        std::fs::write(&torn, b"{ \"format\": \"match-result\"").expect("torn");

        let page = page(&dir, Some("2026-11-02"), 10).expect("page");
        assert_eq!(page.results.len(), 2);
        assert_eq!(page.results[0].date, "2026-11-03");
        assert_eq!(page.skipped, 1);
        let text = lines(&dir, &page);
        assert!(
            text[0].contains("ctf on harbor  team 1 won (capture_limit_reached)"),
            "{text:?}"
        );
        assert!(text.last().is_some_and(|l| l.contains("1 skipped")), "{text:?}");

        assert!(page_err(&dir, Some("11/02"), 10).contains("--since"));
        assert!(page_err(&dir, None, 0).contains("--limit"));
        let missing = super::page(&dir.join("absent"), None, 5).expect("absent folder");
        assert_eq!(missing.total, 0);
    }

    fn page_err(dir: &Path, since: Option<&str>, limit: usize) -> String {
        page(dir, since, limit)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
    }

    /// `regia server ...` as `main.rs` declares it (this module is also compiled into
    /// integration tests through `#[path]`, where the binary's `Cli` does not exist).
    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        command: Top,
    }

    #[derive(clap::Subcommand)]
    enum Top {
        Server(crate::commands::server::ServerArgs),
    }

    fn server(args: &[&str]) -> crate::commands::server::ServerArgs {
        use clap::Parser;
        let Top::Server(server) = Cli::try_parse_from(args).expect("parse").command;
        server
    }

    #[test]
    fn server_results_parses_as_a_subcommand() {
        let parsed = server(&[
            "regia",
            "server",
            "results",
            "--since",
            "2026-11-18",
            "--limit",
            "5",
        ]);
        match parsed.action {
            Some(ServerAction::Results(args)) => {
                assert_eq!(args.since.as_deref(), Some("2026-11-18"));
                assert_eq!(args.limit, 5);
            }
            None => panic!("no subcommand"),
        }
        let dev = server(&["regia", "server", "--clients", "2"]);
        assert!(
            dev.action.is_none(),
            "the dev loop keeps working without a subcommand"
        );
    }
}
