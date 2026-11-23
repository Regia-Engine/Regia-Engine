// SPDX-License-Identifier: MPL-2.0
//! The one-object `--json` mode of `regia cook`, `regia package` and `regia run`
//! (`docs/specs/21-tooling-and-packaging.md` § 1, ADR-7217).
//!
//! While the mode is on, everything a command would print for a person goes to stderr
//! ([`line`]), child processes (cargo, gradle, the game) write their stdout to stderr too
//! ([`child_stdout`]), and the command ends with exactly one compact JSON object on one stdout
//! line ([`emit`]): `ok: true` and the command's result, or `ok: false` and an `error` object
//! with a lowercase `code` and a `message` ([`failure`]). A tool can read stdout whole, or take
//! its last line, as one object; stderr lines are progress.

use std::sync::atomic::{AtomicBool, Ordering};

/// `println!` for a person's line: stdout normally, stderr in the one-object mode ([`line`]).
macro_rules! outln {
    ($($arg:tt)*) => {
        $crate::json_out::line(format_args!($($arg)*))
    };
}

/// `print!` for a person's text: stdout normally, stderr in the one-object mode ([`text`]).
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::json_out::text(format_args!($($arg)*))
    };
}

static JSON_STDOUT: AtomicBool = AtomicBool::new(false);

/// Turns the one-object mode on or off for this process.
pub fn set(on: bool) {
    JSON_STDOUT.store(on, Ordering::Relaxed);
}

/// Whether the one-object mode is on.
pub fn on() -> bool {
    JSON_STDOUT.load(Ordering::Relaxed)
}

/// Prints one line for a person: stdout normally, stderr in the one-object mode.
pub fn line(text: impl std::fmt::Display) {
    if on() {
        eprintln!("{text}");
    } else {
        println!("{text}");
    }
}

/// Prints text for a person without a newline: stdout normally, stderr in the one-object mode.
pub fn text(text: impl std::fmt::Display) {
    if on() {
        eprint!("{text}");
    } else {
        print!("{text}");
    }
}

/// Where a child process's stdout goes: inherited normally, stderr in the one-object mode.
pub fn child_stdout() -> std::process::Stdio {
    if on() {
        std::io::stderr().into()
    } else {
        std::process::Stdio::inherit()
    }
}

/// Prints `value` as the command's one compact JSON object on one stdout line.
pub fn emit(value: &serde_json::Value) {
    println!("{value}");
}

/// The object of a failed command: `ok: false` and `error: { code, message }`.
pub fn failure(code: &str, err: &anyhow::Error) -> serde_json::Value {
    if let Some(integrity) = err.downcast_ref::<crate::commands::content::ContentIntegrityError>() {
        return serde_json::json!({
            "ok": false,
            "error": { "code": integrity.reason, "message": format!("{err:#}"),
                "problems": integrity.problems },
        });
    }
    serde_json::json!({
        "ok": false,
        "error": { "code": code, "message": format!("{err:#}") },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_object_names_the_code_and_the_message() {
        let err = anyhow::anyhow!("no Regia.toml").context("package");
        let value = failure("package_failed", &err);
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "package_failed");
        assert_eq!(value["error"]["message"], "package: no Regia.toml");
        // One line: a tool can take the last stdout line as the object.
        assert!(!value.to_string().contains('\n'));
    }
}
