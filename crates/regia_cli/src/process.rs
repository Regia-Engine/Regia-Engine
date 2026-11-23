// SPDX-License-Identifier: MPL-2.0
//! Running external tools (cargo, gradle, adb) with readable errors.

use std::io::ErrorKind;
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow, bail};

/// Renders a command line for logs and error messages.
pub fn command_line(cmd: &Command) -> String {
    let mut line = cmd.get_program().to_string_lossy().into_owned();
    for arg in cmd.get_args() {
        let arg = arg.to_string_lossy();
        line.push(' ');
        if arg.is_empty() || arg.contains(char::is_whitespace) {
            line.push('"');
            line.push_str(&arg);
            line.push('"');
        } else {
            line.push_str(&arg);
        }
    }
    line
}

/// Like [`command_line`], with every argument equal to one of `secrets` shown as `[redacted]`
/// (empty secrets are ignored).
pub fn command_line_redacted(cmd: &Command, secrets: &[&str]) -> String {
    let mut line = cmd.get_program().to_string_lossy().into_owned();
    for arg in cmd.get_args() {
        let arg = arg.to_string_lossy();
        line.push(' ');
        if !arg.is_empty() && secrets.contains(&arg.as_ref()) {
            line.push_str("[redacted]");
        } else if arg.is_empty() || arg.contains(char::is_whitespace) {
            line.push('"');
            line.push_str(&arg);
            line.push('"');
        } else {
            line.push_str(&arg);
        }
    }
    line
}

/// `line` is the command line to show (already redacted where needed).
fn spawn_error(cmd: &Command, line: &str, err: std::io::Error, fix: &str) -> anyhow::Error {
    if err.kind() == ErrorKind::NotFound {
        anyhow!("`{}` was not found. {fix}", cmd.get_program().to_string_lossy())
    } else {
        anyhow!(err).context(format!("cannot start `{line}`"))
    }
}

/// Runs `cmd` with inherited stdio and fails unless it exits successfully.
/// `fix` is the hint shown when the program does not exist.
pub fn run(cmd: &mut Command, fix: &str) -> Result<()> {
    run_redacted(cmd, fix, &[])
}

/// Like [`run`], with every argument equal to one of `secrets` shown as `[redacted]` in the log
/// line and in the error (see [`command_line_redacted`]).
pub fn run_redacted(cmd: &mut Command, fix: &str, secrets: &[&str]) -> Result<()> {
    let line = command_line_redacted(cmd, secrets);
    tracing::info!("running: {line}");
    // `--json` keeps stdout for the one result object: a child's stdout goes to stderr (ADR-7217).
    if crate::json_out::on() {
        cmd.stdout(crate::json_out::child_stdout());
    }
    let status = cmd.status().map_err(|e| spawn_error(cmd, &line, e, fix))?;
    if !status.success() {
        bail!("`{line}` failed ({status})");
    }
    Ok(())
}

/// Runs `cmd`, capturing stdout and stderr, and returns them on success.
pub fn output(cmd: &mut Command, fix: &str) -> Result<(String, String)> {
    tracing::debug!("running: {}", command_line(cmd));
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| spawn_error(cmd, &command_line(cmd), e, fix))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        bail!(
            "`{}` failed ({}):\n{}{}",
            command_line(cmd),
            out.status,
            stdout,
            stderr
        );
    }
    Ok((stdout, stderr))
}

/// Runs `cmd` capturing output, returning `None` instead of an error when it fails or does not exist.
pub fn try_output(cmd: &mut Command) -> Option<String> {
    let out = cmd.stdin(Stdio::null()).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_quotes_args_with_spaces() {
        let mut cmd = Command::new("adb");
        cmd.args(["-s", "emulator-5554", "install", "/tmp/my game.apk", ""]);
        assert_eq!(
            command_line(&cmd),
            "adb -s emulator-5554 install \"/tmp/my game.apk\" \"\""
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_redacted_keeps_secrets_out_of_the_error() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 3", "hunter2"]);
        let err = run_redacted(&mut cmd, "install sh", &["hunter2"])
            .expect_err("exit 3 fails")
            .to_string();
        assert!(err.contains("[redacted]") && !err.contains("hunter2"), "{err}");
    }

    #[test]
    fn redacted_line_hides_secret_arguments() {
        let mut cmd = Command::new("steamcmd");
        cmd.args([
            "+login",
            "builder",
            "hunter2",
            "+set_steam_guard_code",
            "123456",
            "+quit",
        ]);
        let line = command_line_redacted(&cmd, &["hunter2", "123456", ""]);
        assert_eq!(
            line,
            "steamcmd +login builder [redacted] +set_steam_guard_code [redacted] +quit"
        );
        assert!(!line.contains("hunter2"));
    }

    #[test]
    fn missing_program_gives_fix_it_message() {
        let mut cmd = Command::new("regia-test-definitely-not-a-program");
        let err = run(&mut cmd, "Install it with `pacman -S nothing`.").err();
        let message = err.map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(message.contains("was not found"), "{message}");
        assert!(message.contains("pacman -S nothing"), "{message}");
        assert!(try_output(&mut Command::new("regia-test-definitely-not-a-program")).is_none());
    }
}
