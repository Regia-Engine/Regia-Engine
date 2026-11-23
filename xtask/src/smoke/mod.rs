// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask smoke` — the engine side of the DO27 alpha smoke list.
//!
//! Rows that need a device share one headless session: `lookdev`'s chase soak
//! ([`lookdev::soak::SoakOptions::MINUTE`]) on Switchbacks and Bore Twelve, then a
//! tunnel-interior plate for the mouth cuts, plus a 3840×1600 plate and an airless TAAU
//! camera jump. The many-units row reports encode time and does not fail the run;
//! `moving_units_cpu` gates that slack. `--no-gpu` leaves those rows `N-A`.

mod checks;
mod gpu;
mod report;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Args;

pub use checks::engine_checks;
pub use report::{Check, SmokeReport, Status, write_summary};

/// Arguments for `cargo xtask smoke`.
#[derive(Args, Debug)]
pub struct SmokeArgs {
    /// Directory for `alpha-smoke.json`, `alpha-smoke.md` and `frame.png`.
    /// Defaults to `<workspace>/target/smoke`.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Skip the headless GPU session. Boot, ready, hitches, memory and quit stay `N-A`.
    #[arg(long)]
    pub no_gpu: bool,
    /// Measured seconds of the hitch soak on each map. Clamped to 5..=60.
    /// 60 is [`lookdev::soak::SoakOptions::MINUTE`].
    #[arg(long, default_value_t = 60.0)]
    pub soak_seconds: f32,
    /// Tunnel-plate frames after its warm-up. The soak length comes from [`Self::soak_seconds`].
    #[arg(long, default_value_t = 2)]
    pub frames: u32,
    /// Tunnel-plate frames before it is measured. The soak warm-up is the chase's 2 s.
    #[arg(long, default_value_t = 8)]
    pub warmup: u32,
    /// Frame width for the chase and the tunnel plate.
    #[arg(long, default_value_t = 2560)]
    pub width: u32,
    /// Frame height for the chase and the tunnel plate.
    #[arg(long, default_value_t = 1440)]
    pub height: u32,
    /// DO27 content directory. When omitted, `REGIA_DO27_CONTENT` is used if it is a directory.
    #[arg(long)]
    pub content: Option<PathBuf>,
    /// Run only rows 19–21 (ultrawide, black sky, many units). Skips the map soaks.
    /// A missing content drop fails the ultrawide row and still runs the other two.
    #[arg(long)]
    pub scenes: bool,
}

/// Runs the smoke list and writes the JSON and Markdown summaries.
///
/// # Errors
///
/// The summary files could not be written. A failed check is an exit code, not an error.
pub fn run(args: &SmokeArgs, workspace: &Path) -> Result<ExitCode> {
    let _log = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .try_init();
    let _span = tracing::info_span!("smoke").entered();
    let started = Instant::now();
    let content = resolve_content(args.content.as_deref());
    let mut checks = engine_checks(workspace, content.as_deref());
    let gpu = if args.no_gpu {
        "skipped".to_owned()
    } else if args.scenes {
        let out = args.out.clone().unwrap_or_else(|| workspace.join("target/smoke"));
        match gpu::scenes(&gpu::SessionArgs {
            width: args.width,
            height: args.height,
            warmup: args.warmup,
            frames: args.frames,
            soak_seconds: args.soak_seconds,
            out_dir: &out,
            content: content.as_deref(),
        }) {
            Ok(facts) => {
                let name = facts.adapter.clone();
                checks::apply_scenes(&mut checks, &facts);
                name
            }
            Err(err) => {
                checks::fail_scenes(&mut checks, &err);
                format!("error: {err}")
            }
        }
    } else {
        let out = args.out.clone().unwrap_or_else(|| workspace.join("target/smoke"));
        match gpu::session(&gpu::SessionArgs {
            width: args.width,
            height: args.height,
            warmup: args.warmup,
            frames: args.frames,
            soak_seconds: args.soak_seconds,
            out_dir: &out,
            content: content.as_deref(),
        }) {
            Ok(facts) => {
                let name = facts.adapter.clone();
                checks::apply_gpu(&mut checks, &facts);
                name
            }
            Err(err) => {
                checks::fail_gpu(&mut checks, &err);
                format!("error: {err}")
            }
        }
    };
    let report = SmokeReport {
        finished: finished_stamp(),
        elapsed_s: started.elapsed().as_secs_f64(),
        gpu,
        checks,
    };
    print!("{}", report.text());
    let dir = args.out.clone().unwrap_or_else(|| workspace.join("target/smoke"));
    write_summary(&dir, &report)
        .with_context(|| format!("writing the smoke summary under {}", dir.display()))?;
    println!(
        "wrote {}/alpha-smoke.json and {}/alpha-smoke.md",
        dir.display(),
        dir.display()
    );
    Ok(if report.failed() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// The content directory the checks may read.
///
/// An explicit `--content` path wins. A path that is not a directory means "no content",
/// including when the environment variable is set.
#[must_use]
pub fn resolve_content(flag: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = flag {
        return path.is_dir().then(|| path.to_path_buf());
    }
    std::env::var_os("REGIA_DO27_CONTENT")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
}

fn finished_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|dur| dur.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}
