// SPDX-License-Identifier: MPL-2.0
//! Startup-only UUID lookup (ADR-7958, amended by ADR-7991).
//!
//! Linux reads `/proc/driver/nvidia/gpus/*/information` when `/proc/driver/nvidia` exists and
//! does not execute a process on that path. An exact full UUID still matches when another card's
//! file is malformed, unreadable or uninitialized (`GPU-????…`). A shorter prefix fails closed,
//! and the error names the card. `nvidia-smi` runs on Linux only when `/proc/driver/nvidia` is
//! absent, with the same three-second deadline as other platforms. A `REGIA_GPU_PCI` pin never
//! executes a process; its cross-check reads only that card's information file.
//!
//! Tests inject a temporary proc root and a fallback closure. They must not call `resolve_uuid`
//! or `query_nvidia_smi`: those are the production entry points and would touch a live driver.

#[cfg(any(target_os = "linux", test))]
use std::path::{Path, PathBuf};

/// Internal lookup failures; the existing public request converts these to its string error.
#[derive(Debug, thiserror::Error)]
pub(super) enum UuidError {
    #[cfg(any(target_os = "linux", test))]
    #[error("cannot read NVIDIA card {card} at {path}: {source}; set REGIA_GPU_PCI to pin directly")]
    ProcIo {
        card: String,
        path: PathBuf,
        source: std::io::Error,
    },
    #[cfg(any(target_os = "linux", test))]
    #[error("invalid NVIDIA information field {0}")]
    Information(&'static str),
    #[cfg(any(target_os = "linux", test))]
    #[error("invalid NVIDIA information field {field} on card {card}")]
    CardInformation { field: &'static str, card: String },
    #[cfg(any(target_os = "linux", test))]
    #[error(
        "NVIDIA card {card} has an uninitialized GPU UUID (GPU-????...; persistence mode is off or the card has not been opened); set REGIA_GPU_PCI"
    )]
    Uninitialized { card: String },
    /// Parser-only marker. Directory reads convert this into `UuidError::Uninitialized`.
    #[cfg(any(target_os = "linux", test))]
    #[error("NVIDIA GPU UUID is uninitialized (GPU-????...)")]
    UninitializedUuid,
    #[error("UUID is not listed")]
    NotFound,
    #[error("UUID prefix matches more than one GPU; use a longer prefix")]
    Ambiguous,
    #[cfg(not(target_os = "linux"))]
    #[error("procfs UUID cross-check is only available on Linux")]
    ProcUnavailable,
    #[error("nvidia-smi lookup timed out")]
    Timeout,
    #[error("nvidia-smi lookup failed: {0}")]
    Process(#[from] std::io::Error),
    #[error("nvidia-smi exited with {0}")]
    Exit(std::process::ExitStatus),
    #[error("nvidia-smi returned invalid or excessive output")]
    Output,
}

/// Deadline for the `nvidia-smi` query. Linux uses it only when `/proc/driver/nvidia` is absent.
pub(super) const SMI_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

/// Marker set by a parent test that intentionally spawns the fake child fixtures.
/// `cargo test -- --ignored` does not set it, so those fixtures return immediately.
#[cfg(test)]
pub(super) const TEST_CHILD_ENV: &str = "REGIA_GPU_UUID_TEST_CHILD";

#[cfg(test)]
pub(super) fn test_child_invoked() -> bool {
    std::env::var_os(TEST_CHILD_ENV).is_some_and(|value| !value.is_empty())
}

/// Resolve the UUID using the platform's designated source.
///
/// On Linux this may execute `nvidia-smi` only when `/proc/driver/nvidia` does not exist.
/// Callers that hold `REGIA_GPU_PCI` must use `cross_check_uuid` instead.
pub(super) fn resolve_uuid(uuid: &str) -> Result<String, UuidError> {
    #[cfg(target_os = "linux")]
    {
        resolve_linux(uuid, Path::new("/proc/driver/nvidia"), query_nvidia_smi)
    }
    #[cfg(not(target_os = "linux"))]
    {
        query_nvidia_smi(uuid)
    }
}

fn query_nvidia_smi(uuid: &str) -> Result<String, UuidError> {
    let mut command = std::process::Command::new("nvidia-smi");
    command.args(["--query-gpu=uuid,pci.bus_id", "--format=csv,noheader"]);
    let output = command_output(&mut command, SMI_DEADLINE)?;
    resolve_csv(uuid, &output)
}

/// Best-effort cross-check for one PCI pin: never invoke a process on any platform.
///
/// On Linux this reads only `/proc/driver/nvidia/gpus/<that card>/information`.
pub(super) fn cross_check_uuid(uuid: &str, pci: &str) -> Result<String, UuidError> {
    #[cfg(target_os = "linux")]
    {
        cross_check_pinned(uuid, pci, Path::new("/proc/driver/nvidia/gpus"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (uuid, pci);
        Err(UuidError::ProcUnavailable)
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct GpuInformation {
    pub(super) uuid: String,
    pub(super) pci: String,
    pub(super) device_minor: u32,
}

/// Parse the documented colon-separated information layout, tolerating unrelated fields.
#[cfg(any(target_os = "linux", test))]
pub(super) fn parse_information(text: &str) -> Result<GpuInformation, UuidError> {
    let mut uuid = None;
    let mut pci = None;
    let mut minor = None;
    for (key, value) in text.lines().filter_map(|line| line.split_once(':')) {
        let (slot, field) = match key.trim() {
            "GPU UUID" => (&mut uuid, "GPU UUID"),
            "Bus Location" => (&mut pci, "Bus Location"),
            "Device Minor" => (&mut minor, "Device Minor"),
            _ => continue,
        };
        if slot.replace(value.trim()).is_some() {
            return Err(UuidError::Information(field));
        }
    }
    if uuid.is_some_and(uninitialized_uuid) {
        return Err(UuidError::UninitializedUuid);
    }
    let uuid = uuid
        .filter(|value| valid_uuid(value))
        .ok_or(UuidError::Information("GPU UUID"))?;
    let pci = pci
        .filter(|value| valid_pci(value))
        .ok_or(UuidError::Information("Bus Location"))?;
    let device_minor = minor
        .and_then(|value| value.parse().ok())
        .ok_or(UuidError::Information("Device Minor"))?;
    Ok(GpuInformation {
        uuid: uuid.to_owned(),
        pci: pci.to_owned(),
        device_minor,
    })
}

fn uuid_body_is_full(body: &str) -> bool {
    body.len() == 36
        && body.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn valid_uuid(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    let Some(id) = value.strip_prefix("gpu-") else {
        return false;
    };
    uuid_body_is_full(id)
}

/// `GPU-????????-????-????-????-????????????`, or any placeholder made only of `?` and `-`.
#[cfg(any(target_os = "linux", test))]
fn uninitialized_uuid(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    let id = value.strip_prefix("gpu-").unwrap_or(&value);
    !id.is_empty() && id.contains('?') && id.bytes().all(|byte| byte == b'?' || byte == b'-')
}

/// The requested id is a full NVIDIA UUID, so another card cannot make it ambiguous.
#[cfg(any(target_os = "linux", test))]
fn full_uuid(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    let id = value.strip_prefix("gpu-").unwrap_or(&value);
    uuid_body_is_full(id)
}

fn valid_pci(value: &str) -> bool {
    value.bytes().filter(|b| *b == b':').count() == 2
        && super::parse_pci(value)
            .is_some_and(|(_, bus, device, function)| bus <= 0xff && device <= 0x1f && function <= 7)
}

/// Match without relying on directory enumeration order. Even identical duplicate matches
/// are rejected: silently choosing one can select a card another process owns.
fn choose_uuid<'a>(
    wanted: &str,
    entries: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<String, UuidError> {
    let wanted = wanted.trim().to_ascii_lowercase();
    let wanted = wanted.strip_prefix("gpu-").unwrap_or(&wanted);
    if wanted.is_empty() {
        return Err(UuidError::NotFound);
    }
    let mut found = None;
    for (uuid, pci) in entries {
        let uuid = uuid.to_ascii_lowercase();
        let uuid = uuid.strip_prefix("gpu-").unwrap_or(&uuid);
        if uuid.starts_with(wanted) && found.replace(pci).is_some() {
            return Err(UuidError::Ambiguous);
        }
    }
    found.map(str::to_owned).ok_or(UuidError::NotFound)
}

#[cfg(any(target_os = "linux", test))]
fn card_name(path: &Path) -> String {
    let leaf = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if leaf == "information" {
        path.parent()
            .and_then(|parent| parent.file_name())
            .map(|parent| parent.to_string_lossy().into_owned())
            .filter(|parent| !parent.is_empty())
            .unwrap_or(leaf)
    } else if leaf.is_empty() {
        path.display().to_string()
    } else {
        leaf
    }
}

#[cfg(any(target_os = "linux", test))]
fn proc_io(path: &Path, source: std::io::Error) -> UuidError {
    UuidError::ProcIo {
        card: card_name(path),
        path: path.to_owned(),
        source,
    }
}

/// Directory names to try for one PCI pin, without listing any other card.
///
/// The kernel names the proc directory `dddd:bb:dd.f` (`%04x:%02x:%02x.%x`). `nvidia-smi` often
/// prints an eight-digit domain. Both spellings refer to the same card.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn pci_directory_names(pci: &str) -> Vec<String> {
    let trimmed = pci.trim();
    let mut names = Vec::new();
    if !trimmed.is_empty() {
        names.push(trimmed.to_owned());
    }
    let Some((domain, bus, device, function)) = super::parse_pci(trimmed) else {
        return names;
    };
    let kernel = format!("{domain:04x}:{bus:02x}:{device:02x}.{function:x}");
    if !names.iter().any(|name| name.eq_ignore_ascii_case(&kernel)) {
        names.push(kernel);
    }
    let wide = format!("{domain:08x}:{bus:02x}:{device:02x}.{function:x}");
    if !names.iter().any(|name| name.eq_ignore_ascii_case(&wide)) {
        names.push(wide);
    }
    names
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn warn_uninitialized(card: &str, pci: &str) {
    tracing::warn!(
        target: "regia_render",
        "NVIDIA card {card} has an uninitialized GPU UUID (GPU-????...; persistence mode is off or the card has not been opened); {pci_env}={pci} still wins",
        pci_env = super::GPU_PCI_ENV,
    );
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn pinned_information(uuid: &str, pci: &str, card: &str, text: &str) -> Result<String, UuidError> {
    let info = match parse_information(text) {
        Ok(info) => info,
        Err(UuidError::UninitializedUuid) => {
            warn_uninitialized(card, pci);
            return Err(UuidError::Uninitialized {
                card: card.to_owned(),
            });
        }
        Err(UuidError::Information(field)) => {
            return Err(UuidError::CardInformation {
                field,
                card: card.to_owned(),
            });
        }
        Err(other) => return Err(other),
    };
    match choose_uuid(uuid, std::iter::once((info.uuid.as_str(), info.pci.as_str()))) {
        Ok(resolved) => Ok(resolved),
        Err(UuidError::NotFound) => {
            tracing::warn!(
                target: "regia_render",
                "{uuid_env}={wanted} does not match card {card} ({found}); {pci_env}={pci} wins",
                uuid_env = super::GPU_UUID_ENV,
                wanted = uuid.trim(),
                found = info.uuid,
                pci_env = super::GPU_PCI_ENV,
            );
            Err(UuidError::NotFound)
        }
        Err(other) => Err(other),
    }
}

/// Read only `gpus_root/<pci>/information` (and the other spelling of that same bus).
#[cfg(any(target_os = "linux", all(test, unix)))]
pub(super) fn cross_check_pinned(uuid: &str, pci: &str, gpus_root: &Path) -> Result<String, UuidError> {
    let names = pci_directory_names(pci);
    if names.is_empty() {
        return Err(UuidError::NotFound);
    }
    let mut missing = None;
    let mut unreadable = None;
    for name in names {
        let path = gpus_root.join(&name).join("information");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                missing = Some(proc_io(&path, source));
                continue;
            }
            Err(source) => {
                unreadable = Some(proc_io(&path, source));
                continue;
            }
        };
        return pinned_information(uuid, pci, &name, &text);
    }
    Err(unreadable.or(missing).unwrap_or(UuidError::NotFound))
}

#[cfg(any(target_os = "linux", test))]
fn blocking_error(uninitialized: &[String], failures: Vec<(String, UuidError)>) -> Option<UuidError> {
    if !uninitialized.is_empty() {
        return Some(UuidError::Uninitialized {
            card: uninitialized.join(", "),
        });
    }
    failures.into_iter().next().map(|(_, err)| err)
}

/// Read a fixture root or Linux procfs.
///
/// An exact full UUID is returned when one readable card has it, even if a neighbor file is
/// malformed, unreadable or uninitialized. A shorter prefix still requires every card to be
/// readable and valid, because a neighbor could hide another match. Any failure names the card.
#[cfg(any(target_os = "linux", test))]
pub(super) fn resolve_uuid_from_proc(uuid: &str, root: &Path) -> Result<String, UuidError> {
    let entries = std::fs::read_dir(root).map_err(|source| proc_io(root, source))?;
    let mut gpus = Vec::new();
    let mut uninitialized = Vec::new();
    let mut failures = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| proc_io(root, source))?;
        let file_type = entry
            .file_type()
            .map_err(|source| proc_io(&entry.path(), source))?;
        if !file_type.is_dir() {
            continue;
        }
        let card = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path().join("information");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) => {
                failures.push((card, proc_io(&path, source)));
                continue;
            }
        };
        match parse_information(&text) {
            Ok(info) => gpus.push(info),
            Err(UuidError::UninitializedUuid) => uninitialized.push(card),
            Err(UuidError::Information(field)) => {
                failures.push((card.clone(), UuidError::CardInformation { field, card }));
            }
            Err(other) => failures.push((card, other)),
        }
    }
    uninitialized.sort();
    failures.sort_by(|left, right| left.0.cmp(&right.0));
    match choose_uuid(uuid, gpus.iter().map(|gpu| (gpu.uuid.as_str(), gpu.pci.as_str()))) {
        Ok(pci) if full_uuid(uuid) || (uninitialized.is_empty() && failures.is_empty()) => Ok(pci),
        Ok(_) | Err(UuidError::NotFound) => {
            Err(blocking_error(&uninitialized, failures).unwrap_or(UuidError::NotFound))
        }
        Err(other) => Err(other),
    }
}

/// Linux UUID lookup with an injected proc root and an injected `nvidia-smi` fallback.
///
/// The fallback runs only when `nvidia_proc` (`/proc/driver/nvidia` in production) is confirmed
/// absent. A present path, a path that is not a directory, or an error while checking the path
/// never calls `smi`.
#[cfg(any(target_os = "linux", all(test, unix)))]
pub(super) fn resolve_linux(
    uuid: &str,
    nvidia_proc: &Path,
    smi: impl FnOnce(&str) -> Result<String, UuidError>,
) -> Result<String, UuidError> {
    match nvidia_proc.try_exists() {
        Ok(false) => smi(uuid),
        Err(source) => Err(proc_io(nvidia_proc, source)),
        Ok(true) => {
            let meta = std::fs::metadata(nvidia_proc).map_err(|source| proc_io(nvidia_proc, source))?;
            if !meta.is_dir() {
                return Err(proc_io(
                    nvidia_proc,
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "NVIDIA proc path is not a directory",
                    ),
                ));
            }
            resolve_uuid_from_proc(uuid, &nvidia_proc.join("gpus"))
        }
    }
}

fn resolve_csv(uuid: &str, text: &str) -> Result<String, UuidError> {
    let mut entries = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let (id, pci) = line.split_once(',').ok_or(UuidError::Output)?;
        let (id, pci) = (id.trim(), pci.trim());
        if !valid_uuid(id) || !valid_pci(pci) {
            return Err(UuidError::Output);
        }
        entries.push((id, pci));
    }
    choose_uuid(uuid, entries.into_iter())
}

/// Bound the entire wait and output collection, not just the child's exit. The output worker
/// prevents a full pipe from deadlocking the child; its cap bounds allocation. Kill on any
/// error, then reap off-thread so uninterruptible sleep cannot hold the calling host's lock.
pub(super) fn command_output(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
) -> Result<String, UuidError> {
    use std::io::Read as _;
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    const MAX_OUTPUT: u64 = 64 * 1024;
    let started = Instant::now();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| {
        let stdout = child.stdout.take().ok_or(UuidError::Output)?;
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("gpu-uuid-output".into())
            .spawn(move || {
                let mut bytes = Vec::new();
                let result = stdout.take(MAX_OUTPUT + 1).read_to_end(&mut bytes).map(|_| bytes);
                let _ = sender.send(result);
            })?;
        loop {
            let remaining = timeout
                .checked_sub(started.elapsed())
                .filter(|d| !d.is_zero())
                .ok_or(UuidError::Timeout)?;
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(UuidError::Exit(status));
                }
                let bytes = receiver.recv_timeout(remaining).map_err(|err| match err {
                    mpsc::RecvTimeoutError::Timeout => UuidError::Timeout,
                    mpsc::RecvTimeoutError::Disconnected => UuidError::Output,
                })??;
                if bytes.len() as u64 > MAX_OUTPUT {
                    return Err(UuidError::Output);
                }
                return String::from_utf8(bytes).map_err(|_| UuidError::Output);
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        // Child::wait is deliberately absent from the caller thread, even after kill.
        let _ = std::thread::Builder::new()
            .name("gpu-uuid-reap".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smi_deadline_is_three_seconds() {
        assert_eq!(SMI_DEADLINE, std::time::Duration::from_secs(3));
    }

    #[test]
    fn csv_fallback_validates_fields_and_unique_prefixes() {
        let csv = "GPU-12345678-1234-5678-9abc-123456789abc, 00000000:21:00.0\r\nGPU-12345679-1234-5678-9abc-123456789abc, 00000000:01:00.0\r\n";
        assert_eq!(resolve_csv("gpu-12345678", csv).expect("CSV"), "00000000:21:00.0");
        assert!(matches!(resolve_csv("1234567", csv), Err(UuidError::Ambiguous)));
        assert!(matches!(resolve_csv("GPU-", csv), Err(UuidError::NotFound)));
        for invalid in [
            "not a CSV row",
            "GPU-invalid, 0000:21:00.0",
            "GPU-12345678-1234-5678-9abc-123456789abc, unknown",
        ] {
            assert!(matches!(resolve_csv("1234", invalid), Err(UuidError::Output)));
        }
    }

    #[test]
    fn subprocess_success_and_nonzero_exit_are_distinct() {
        let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
        command.args(["--list", "gpu_select::regression_tests::sleeping_fake_binary"]);
        assert!(
            command_output(&mut command, std::time::Duration::from_secs(10))
                .expect("successful fake binary")
                .contains("sleeping_fake_binary")
        );
        let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
        command.arg("--invalid-test-harness-argument");
        assert!(matches!(
            command_output(&mut command, std::time::Duration::from_secs(10)),
            Err(UuidError::Exit(_))
        ));
    }

    #[test]
    fn missing_binary_is_a_typed_process_error() {
        let root = regia_test_utils::TempDir::new("uuid-missing-binary");
        let mut command = std::process::Command::new(root.join("not-a-program"));
        assert!(matches!(
            command_output(&mut command, std::time::Duration::from_secs(1)),
            Err(UuidError::Process(_))
        ));
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[test]
    fn output_is_capped_and_must_be_utf8() {
        for mode in ["oversize", "invalid_utf8"] {
            let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
            command.args([
                "--ignored",
                "--exact",
                "--nocapture",
                "gpu_select::uuid::output_tests::fake_output_binary",
            ]);
            command.env(TEST_CHILD_ENV, "1");
            command.env("REGIA_GPU_UUID_TEST_OUTPUT", mode);
            assert!(
                matches!(
                    command_output(&mut command, std::time::Duration::from_secs(10)),
                    Err(UuidError::Output)
                ),
                "{mode}"
            );
        }
    }

    #[test]
    #[ignore = "fake subprocess output fixture; invoked by output_is_capped_and_must_be_utf8"]
    fn fake_output_binary() {
        use std::io::Write as _;
        // `--ignored` runs this test by itself. Without the parent marker, return at once
        // instead of panicking or writing a payload.
        if !test_child_invoked() {
            return;
        }
        let bytes = match std::env::var("REGIA_GPU_UUID_TEST_OUTPUT").as_deref() {
            Ok("oversize") => vec![b'x'; 64 * 1024 + 1],
            Ok("invalid_utf8") => vec![0xff],
            _ => panic!("fake subprocess mode missing"),
        };
        // A capped reader can close its pipe during this write. Exit cleanly either way so
        // the parent tests output validation independently of unsuccessful process status.
        let _ = std::io::stdout().lock().write_all(&bytes);
        std::process::exit(0);
    }
}
