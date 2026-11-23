// SPDX-License-Identifier: MPL-2.0
//! `regia golden list | accept <id> | reject <id>`: the owner's side of golden proposals (ADR-2821,
//! `book/agents/goldens.md`).
//!
//! An agent proposes with `agent.bless_golden`; no protocol method can accept. `accept` is for a
//! person at a terminal: it refuses unless stdin and stderr are terminals, shows what is proposed,
//! and asks for the golden's name to be typed. There is no flag and no environment variable that
//! skips the question, and `REGIA_BLESS` plays no part. Accepting installs the proposal's exact bytes
//! and adds the reason to `.regia/agent/review/golden_commit_message.txt` for `git commit -F`.

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use regia_agent::golden_tools::{self, OwnerConfirmation, plain_text};
use regia_agent::review::StagedChange;
use serde_json::{Value, json};

/// `regia golden` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum GoldenCommand {
    /// List the pending golden proposals.
    List(GoldenListArgs),
    /// Install a proposal's image as the golden. Only at a terminal: you type the golden's name.
    Accept(GoldenDecideArgs),
    /// Drop a proposal. The golden does not change.
    Reject(GoldenRejectArgs),
}

/// Arguments of `regia golden list`.
#[derive(Debug, Clone, Args)]
pub struct GoldenListArgs {
    /// Project directory.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments of `regia golden accept`.
#[derive(Debug, Clone, Args)]
pub struct GoldenDecideArgs {
    /// The proposal's change id (`regia golden list`, `review.list`).
    #[arg(value_name = "ID")]
    pub id: u64,
    /// Project directory.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
}

/// Arguments of `regia golden reject`.
#[derive(Debug, Clone, Args)]
pub struct GoldenRejectArgs {
    /// The proposal's change id.
    #[arg(value_name = "ID")]
    pub id: u64,
    /// Project directory.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Why, for the queue's record.
    #[arg(long)]
    pub note: Option<String>,
}

/// Runs `regia golden`.
///
/// # Errors
/// A refused decision, named by its code.
pub fn run(command: &GoldenCommand) -> Result<()> {
    match command {
        GoldenCommand::List(args) => list(args),
        GoldenCommand::Accept(args) => accept(args),
        GoldenCommand::Reject(args) => {
            golden_tools::owner_reject(&args.project, args.id, args.note.as_deref())?;
            println!("rejected change {}; the golden did not change", args.id);
            Ok(())
        }
    }
}

/// What a person reads before deciding.
fn described(change: &StagedChange) -> String {
    let golden = change.golden.as_ref();
    let dir = plain_text(golden.map_or("", |g| g.dir.as_str()));
    let stats = golden
        .and_then(|g| serde_json::from_str::<Value>(&g.stats).ok())
        .unwrap_or(Value::Null);
    let measured: Vec<String> = ["ssim", "psnr", "max_channel_diff", "mean_lum", "object_pct"]
        .iter()
        .filter_map(|key| {
            stats
                .get(*key)
                .filter(|v| !v.is_null())
                .map(|v| format!("{key} {v}"))
        })
        .collect();
    format!(
        "change {id}: {path}\n  reason:   {reason}\n  proposed: {client} {model}\n  measured: {measured}\n  before:   {dir}/before.png\n  after:    {dir}/after.png\n  diff:     {dir}/diff.png\n",
        id = change.id,
        path = plain_text(&change.path),
        reason = plain_text(&change.reason),
        client = plain_text(&change.client),
        model = plain_text(&change.model),
        measured = plain_text(&measured.join(", ")),
    )
}

fn list(args: &GoldenListArgs) -> Result<()> {
    let pending = golden_tools::pending_proposals(&args.project)?;
    if args.json {
        let rows: Vec<Value> = pending.iter().map(StagedChange::summary).collect();
        println!("{}", json!({ "pending": rows }));
        return Ok(());
    }
    if pending.is_empty() {
        println!("no golden proposals are pending");
    }
    for change in &pending {
        println!("{}", described(change));
    }
    Ok(())
}

fn accept(args: &GoldenDecideArgs) -> Result<()> {
    // Checked first, so a process without a terminal learns nothing and changes nothing.
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!(
            "golden_needs_terminal: only a person at a terminal accepts a golden; run `regia golden accept {}` there (ADR-2821)",
            args.id
        );
    }
    let change = golden_tools::proposal(&args.project, args.id)?;
    let stem = change
        .golden
        .as_ref()
        .map_or("", |g| g.name.rsplit(':').next().unwrap_or(&g.name))
        .to_string();
    let mut err = std::io::stderr().lock();
    writeln!(err, "{}", described(&change))?;
    write!(err, "Look at the images, then type {stem} to bless it: ")?;
    err.flush()?;
    drop(err);
    let mut typed = String::new();
    std::io::stdin().lock().read_line(&mut typed)?;
    let accepted = golden_tools::owner_accept(&args.project, args.id, &OwnerConfirmation::typed(&typed))?;
    println!("blessed {} ({})", accepted.name, accepted.path);
    println!(
        "commit it with the reason: git commit -F {} (then remove that file)",
        golden_tools::commit_message_path(&args.project).display()
    );
    Ok(())
}
