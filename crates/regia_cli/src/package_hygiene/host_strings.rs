// SPDX-License-Identifier: MPL-2.0
//! Printable executable-string gate, original Rust (ADR-10354).

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use regex::bytes::Regex;
use serde::Deserialize;

use super::{AuditError, Finding, Policy, Report};

/// The private build-machine names this gate refuses, XOR-masked with [`MASK`]: the shipped tool
/// must not carry the names it looks for as printable text, or `strings` on the tool itself would
/// list them (ADR-10354). The plain compound
/// (row 3) matches at token boundaries: `strings` joins adjacent literals, and a PE stores the
/// channel labels "left main" and "right main" back to back (`mainright`), which names no host.
pub(super) const PRIVATE: [(&[u8], bool); 8] = [
    (b"\xd3\xc4\xc9\xce\xdc\xd7\xcc\xc0", false),
    (b"\xc8\xc4\xcc\xcb\x88\xd7\xcc\xc2", false),
    (b"\xc8\xc4\xcc\xcb\xd7\xcc\xc2", true),
    (b"\xd1\xc0\xd6\xd1\x88\xd7\xcc\xc2", false),
    (b"\xcb\xd3\xc7\xca\xdd", false),
    (b"\xc9\xc4\xd5\xd1\xca\xd5\x88\xc8\xc4\xc6", false),
    (b"\xc8\xcc\xcb\xcc\x88\xc9\xc4\xd5\xd1\xca\xd5", false),
    (b"\xd5\xcc\xc0\xd7\xc6\xcc\xcb\xc2\xdd\xdd", false),
];

/// The XOR mask of [`PRIVATE`]. Every masked byte is outside the printable ASCII range.
const MASK: u8 = 0xA5;

/// The generic directory roots, LAN range and tailnet names: public patterns, kept in plain text.
/// `tailscale` stays a bare substring so `tailscaled` is still caught.
const ROOTS: &str = r"/media/|/srv/|/models/|/home/|192\.168\.|\.ts\.net|tailscale";

/// The case-insensitive pattern of the gate, assembled at runtime.
pub(super) fn pattern_source() -> String {
    let names: Vec<String> = PRIVATE
        .iter()
        .map(|(name, bounded)| {
            let clear: Vec<u8> = name.iter().map(|byte| byte ^ MASK).collect();
            let clear = String::from_utf8_lossy(&clear).into_owned();
            if *bounded { format!(r"\b{clear}\b") } else { clear }
        })
        .collect();
    format!(r"(?i-u){}|{ROOTS}", names.join("|"))
}

const MAX_ALLOW_BYTES: u64 = 1 << 20;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AllowFile {
    allow: Vec<Allowance>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Allowance {
    path: String,
    text: String,
    reason: String,
}

pub(super) struct Scanner {
    pub(super) pattern: Regex,
    allow: AllowFile,
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.components().all(|part| matches!(part, Component::Normal(_)))
        && !path.to_string_lossy().contains(['\\', ':'])
}

/// Resolve an optional allow-list without adding fields to the public manifest structs.
pub(super) fn project_allow_file(project: &Path) -> Result<Option<PathBuf>, AuditError> {
    #[derive(Default, Deserialize)]
    struct Package {
        host_strings_allow: Option<PathBuf>,
    }
    #[derive(Deserialize)]
    struct Manifest {
        #[serde(default)]
        package: Package,
    }
    let manifest: Manifest = toml::from_str(&super::policy::read_manifest(project)?)
        .map_err(|_| AuditError::config("package.host_strings_allow"))?;
    let Some(relative) = manifest.package.host_strings_allow else {
        return Ok(None);
    };
    if !safe_relative(&relative) {
        return Err(AuditError::config("package.host_strings_allow"));
    }
    let root = project
        .canonicalize()
        .map_err(|_| AuditError::new("package_hygiene_io", "host_strings_allow"))?;
    let path = root
        .join(relative)
        .canonicalize()
        .map_err(|_| AuditError::new("package_hygiene_io", "host_strings_allow"))?;
    if !path.starts_with(&root) {
        return Err(AuditError::config("package.host_strings_allow"));
    }
    Ok(Some(path))
}

impl Scanner {
    pub(super) fn new(allow_file: Option<&Path>) -> Result<Self, AuditError> {
        let allow = if let Some(path) = allow_file {
            let file = std::fs::File::open(path)
                .map_err(|_| AuditError::new("package_hygiene_io", "host_strings_allow"))?;
            let mut text = String::new();
            file.take(MAX_ALLOW_BYTES + 1)
                .read_to_string(&mut text)
                .map_err(|_| AuditError::new("package_hygiene_io", "host_strings_allow"))?;
            if text.len() as u64 > MAX_ALLOW_BYTES {
                return Err(AuditError::config("host_strings_allow_size"));
            }
            toml::from_str::<AllowFile>(&text).map_err(|_| AuditError::config("host_strings_allow"))?
        } else {
            AllowFile::default()
        };
        if allow.allow.len() > 1024
            || allow.allow.iter().any(|entry| {
                !safe_relative(Path::new(&entry.path))
                    || entry.reason.trim().is_empty()
                    || entry.text.len() < 6
                    || !entry.text.bytes().all(printable)
            })
        {
            return Err(AuditError::config("host_strings_allow"));
        }
        Ok(Self {
            pattern: Regex::new(&pattern_source()).map_err(|_| AuditError::config("host_string_pattern"))?,
            allow,
        })
    }

    pub(super) fn scan(
        &self,
        data: &[u8],
        path: &str,
        location_id: &str,
        policy: &Policy,
        report: &mut Report,
    ) -> Result<(), AuditError> {
        let mut origin = 0;
        for text in data.split(|&byte| !printable(byte)) {
            if text.len() >= 6 {
                for hit in self.pattern.find_iter(text) {
                    if self
                        .allow
                        .allow
                        .iter()
                        .any(|entry| entry.path == path && entry.text.as_bytes() == text)
                    {
                        report.allowed_findings = report.allowed_findings.saturating_add(1);
                        continue;
                    }
                    if report.findings.len() >= policy.max_findings {
                        report.complete = false;
                        report.incomplete_reason = "finding_cap".into();
                        return Err(AuditError {
                            report: Some(Box::new(report.clone())),
                            ..AuditError::new("package_hygiene_limit", "host_string")
                        });
                    }
                    report.findings.push(Finding {
                        // Do not echo a filename that might itself contain a private host.
                        path: "[executable]".into(),
                        location_id: location_id.into(),
                        rule: "host_string".into(),
                        source: "content".into(),
                        offset: origin + hit.start(),
                    });
                }
            }
            origin += text.len() + 1;
        }
        Ok(())
    }
}

fn printable(byte: u8) -> bool {
    byte.is_ascii_graphic() || matches!(byte, b' ' | b'\t')
}

fn executable(data: &[u8]) -> bool {
    data.starts_with(b"\x7fELF")
        || data.starts_with(b"MZ")
        || data.starts_with(b"\0asm")
        || [
            [0xfe, 0xed, 0xfa, 0xce],
            [0xce, 0xfa, 0xed, 0xfe],
            [0xfe, 0xed, 0xfa, 0xcf],
            [0xcf, 0xfa, 0xed, 0xfe],
            [0xca, 0xfe, 0xba, 0xbe],
            [0xbe, 0xba, 0xfe, 0xca],
            [0xca, 0xfe, 0xba, 0xbf],
            [0xbf, 0xba, 0xfe, 0xca],
        ]
        .iter()
        .any(|magic| data.starts_with(magic))
}

/// Whether a staged file starts with an executable magic number, reading its first bytes only: a
/// package holds far more data than code, and every asset is already read by the content audit.
pub(super) fn is_executable_file(path: &Path) -> Result<bool, AuditError> {
    let file = std::fs::File::open(path).map_err(|_| AuditError::new("package_hygiene_io", "executable"))?;
    let mut head = Vec::new();
    file.take(8)
        .read_to_end(&mut head)
        .map_err(|_| AuditError::new("package_hygiene_io", "executable"))?;
    Ok(executable(&head))
}

/// Check an executable's printable strings with the packaging gate (ADR-10354).
///
/// This scans all bytes, including nonloaded sections, as `LC_ALL=C strings -a -n 6`
/// does. The optional TOML allow-list contains `[[allow]]` entries with an exact
/// `path` (the binary's basename), exact printable `text`, and nonblank `reason`.
/// No exceptions are enabled by default. ELF, PE, Mach-O and WebAssembly are accepted;
/// unrecognized files fail closed. No executable is run or modified.
///
/// # Errors
/// An unreadable or oversized file, invalid allow-list, unrecognized executable,
/// or a host string. Errors contain offsets and rule IDs, never matched text.
pub fn check_executable_strings(binary: &Path, allow_file: Option<&Path>) -> Result<Report, AuditError> {
    let policy = Policy::default();
    let scanner = Scanner::new(allow_file)?;
    let meta =
        std::fs::symlink_metadata(binary).map_err(|_| AuditError::new("package_hygiene_io", "executable"))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(AuditError::new("package_hygiene_path", "executable"));
    }
    if meta.len() > policy.max_file_bytes {
        return Err(AuditError::new("package_hygiene_limit", "executable"));
    }
    let mut data = Vec::new();
    std::fs::File::open(binary)
        .and_then(|file| file.take(meta.len() + 1).read_to_end(&mut data))
        .map_err(|_| AuditError::new("package_hygiene_io", "executable"))?;
    if data.len() as u64 != meta.len() {
        return Err(AuditError::new("package_hygiene_io", "executable_changed"));
    }
    if !executable(&data) {
        return Err(AuditError::new("package_hygiene_path", "executable_format"));
    }
    let name = binary
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| AuditError::new("package_hygiene_path", "executable"))?;
    let mut report = Report {
        complete: true,
        files_scanned: 1,
        bytes_scanned: data.len() as u64,
        ..Report::default()
    };
    scanner.scan(&data, name, "executable-1", &policy, &mut report)?;
    reject(report)
}

fn reject(report: Report) -> Result<Report, AuditError> {
    if report.findings.is_empty() {
        Ok(report)
    } else {
        Err(AuditError {
            report: Some(Box::new(report)),
            ..AuditError::new(
                "package_hygiene_failed",
                "executable contains private host strings; rebuild with source-path remapping or review an exact host_strings_allow exception",
            )
        })
    }
}
