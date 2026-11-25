// SPDX-License-Identifier: MPL-2.0
//! `regia package parity` (ADR-8428).
//!
//! Offline diff of two access traces, or two scripted runs with
//! `REGIA_ASSET_TRACE` set. `--classify` is the strace open-trace report.

use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicU64, Ordering};

use regia_asset::package_parity::{ParityReport, diff_paths_mode, open_trace_main};

static SEQ: AtomicU64 = AtomicU64::new(0);

const HELP: &str = "\
usage: regia package parity [options]

Compare a dev run with an extracted package along one scripted path.

Examples:
  regia package parity --dev-trace dev.log --dev-root . --package-trace pkg.log --package-root out/pkg
  regia package parity --dev-command 'regia run' --package-command './my_game' --dev-root . --package-root out/pkg --json
  regia package parity --classify --trace run.strace --root out/pkg

Exit codes:
  0  the package accounts for every dev open
  1  a dev open is missing from the package (drift does not fail unless --strict-drift)
  2  usage error, parity_script_missing, or parity_run_failed

CLI exits parity_script_missing and parity_run_failed are not agent error codes.
";

struct Args {
    dev_trace: Option<String>,
    dev_root: Option<String>,
    package_trace: Option<String>,
    package_root: Option<String>,
    dev_command: Option<String>,
    package_command: Option<String>,
    script: Option<String>,
    report_md: Option<String>,
    report_json: Option<String>,
    json: bool,
    strict_drift: bool,
}

/// Run `package parity`. `args` is everything after those two words.
#[must_use]
pub fn run(args: &[String]) -> ExitCode {
    ExitCode::from(run_code(args) as u8)
}

fn run_code(args: &[String]) -> i32 {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") && !args.iter().any(|arg| arg == "--classify") {
        print!("{HELP}");
        return 0;
    }
    if args.iter().any(|arg| arg == "--classify") {
        let rest: Vec<String> = args
            .iter()
            .filter(|arg| arg.as_str() != "--classify")
            .cloned()
            .collect();
        let (code, stdout, stderr) = open_trace_main(&rest);
        print!("{stdout}");
        eprint!("{stderr}");
        return code;
    }
    let args = match parse(args) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("error: {message}");
            return 2;
        }
    };
    if let Some(script) = &args.script
        && !Path::new(script).is_file()
    {
        eprintln!("error: parity_script_missing: {script}");
        return 2;
    }
    let dev_root = match &args.dev_root {
        Some(path) => path.clone(),
        None => {
            eprintln!("error: --dev-root is required");
            return 2;
        }
    };
    let package_root = match &args.package_root {
        Some(path) => path.clone(),
        None => {
            eprintln!("error: --package-root is required");
            return 2;
        }
    };
    let dev_trace = args.dev_trace.clone().unwrap_or_else(|| temp_trace("dev"));
    let package_trace = args.package_trace.clone().unwrap_or_else(|| temp_trace("pkg"));
    if args.dev_command.is_some() || args.package_command.is_some() {
        let (Some(dev_command), Some(package_command)) = (&args.dev_command, &args.package_command) else {
            eprintln!("error: --dev-command and --package-command are both required");
            return 2;
        };
        if let Err(code) = run_side(dev_command, &dev_trace, &dev_root, args.script.as_deref(), "dev") {
            return code;
        }
        if let Err(code) = run_side(
            package_command,
            &package_trace,
            &package_root,
            args.script.as_deref(),
            "package",
        ) {
            return code;
        }
    } else if args.dev_trace.is_none() || args.package_trace.is_none() {
        eprintln!("error: pass both traces, or both commands");
        return 2;
    }
    let report = match diff_paths_mode(
        Path::new(&dev_trace),
        Path::new(&dev_root),
        Path::new(&package_trace),
        Path::new(&package_root),
        args.strict_drift,
    ) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("error: {err}");
            return 2;
        }
    };
    emit(&args, &report);
    report.exit_code
}

fn emit(args: &Args, report: &ParityReport) {
    if let Some(path) = &args.report_md
        && let Err(err) = write_atomic(Path::new(path), report.markdown.as_bytes())
    {
        eprintln!("error: report markdown: {err}");
    }
    let json = serde_json::to_string_pretty(&report.json_value()).unwrap_or_else(|_| "{}".to_string());
    if let Some(path) = &args.report_json
        && let Err(err) = write_atomic(Path::new(path), format!("{json}\n").as_bytes())
    {
        eprintln!("error: report json: {err}");
    }
    if args.json {
        println!("{json}");
    } else {
        print!("{}", report.markdown);
    }
}

fn run_side(command: &str, trace: &str, root: &str, script: Option<&str>, side: &str) -> Result<(), i32> {
    if let Some(parent) = Path::new(trace).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|err| {
            eprintln!("error: parity_run_failed: {side} trace dir: {err}");
            2
        })?;
    }
    let mut child = if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    };
    child.env("REGIA_ASSET_TRACE", trace);
    child.env("REGIA_PARITY_ROOT", root);
    if let Some(script) = script {
        child.env("REGIA_PARITY_SCRIPT", script);
    }
    match child.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => {
            eprintln!("error: parity_run_failed: {side} exited {status}");
            Err(2)
        }
        Err(err) => {
            eprintln!("error: parity_run_failed: {side}: {err}");
            Err(2)
        }
    }
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut out = Args {
        dev_trace: None,
        dev_root: None,
        package_trace: None,
        package_root: None,
        dev_command: None,
        package_command: None,
        script: None,
        report_md: None,
        report_json: None,
        json: false,
        strict_drift: false,
    };
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let mut take = || -> Result<String, String> {
            i += 1;
            args.get(i).cloned().ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--dev-trace" => out.dev_trace = Some(take()?),
            "--dev-root" => out.dev_root = Some(take()?),
            "--package-trace" => out.package_trace = Some(take()?),
            "--package-root" => out.package_root = Some(take()?),
            "--dev-command" => out.dev_command = Some(take()?),
            "--package-command" => out.package_command = Some(take()?),
            "--script" => out.script = Some(take()?),
            "--report-md" => out.report_md = Some(take()?),
            "--report-json" => out.report_json = Some(take()?),
            "--json" => out.json = true,
            "--strict-drift" => out.strict_drift = true,
            other => return Err(format!("unknown argument {other}")),
        }
        i += 1;
    }
    Ok(out)
}

fn temp_trace(label: &str) -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join(format!("regia-parity-{label}-{}-{n}.trace", std::process::id()))
        .display()
        .to_string()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::run_code;

    #[test]
    fn help_lists_usage_examples_and_exit_codes() {
        let code = run_code(&["--help".to_string()]);
        assert_eq!(code, 0);
    }

    #[test]
    fn offline_diff_exits_when_a_dev_open_is_missing() {
        let tmp = regia_test_utils::TempDir::new("cli-parity");
        let dev = tmp.join("dev");
        let pkg = tmp.join("pkg");
        std::fs::create_dir_all(dev.join("assets")).unwrap_or_else(|err| panic!("{err}"));
        std::fs::create_dir_all(&pkg).unwrap_or_else(|err| panic!("{err}"));
        std::fs::write(dev.join("assets/needed.txt"), b"needed").unwrap_or_else(|err| panic!("{err}"));
        let dev_trace = tmp.join("dev.trace");
        let pkg_trace = tmp.join("pkg.trace");
        std::fs::write(
            &dev_trace,
            format!("ok open {}/assets/needed.txt\n", dev.display()),
        )
        .unwrap_or_else(|err| panic!("{err}"));
        std::fs::write(&pkg_trace, b"").unwrap_or_else(|err| panic!("{err}"));
        let code = run_code(&[
            "--dev-trace".to_string(),
            dev_trace.display().to_string(),
            "--dev-root".to_string(),
            dev.display().to_string(),
            "--package-trace".to_string(),
            pkg_trace.display().to_string(),
            "--package-root".to_string(),
            pkg.display().to_string(),
            "--json".to_string(),
        ]);
        assert_eq!(code, 1);
    }
}
