// SPDX-License-Identifier: MPL-2.0
//! BENCH_GPU5 regressions: synthetic files and injected resolvers only.
use super::*;

// The `#[cfg(unix)]` tests below plant a Linux procfs tree (the reader is Linux-only in production): NTFS
// refuses its card directory names (`0000:21:00.0`), and Windows reports a path under a regular file as absent,
// not unstatable, so they cannot set up their fixture there.
const INFORMATION: &str = include_str!("../../tests/fixtures/nvidia-information.txt");

#[test]
fn explicit_pci_wins_over_conflicting_or_unresolved_uuid() {
    let adapters = vec![
        ("card A".into(), "0000:01:00.0".into()),
        ("card B".into(), "0000:21:00.0".into()),
    ];
    for uuid_pci in [Ok("0000:01:00.0".into()), Err("unavailable".into())] {
        let request = GpuRequest {
            pci: Some("0000:21:00.0".into()),
            uuid_pci: Some(uuid_pci),
            ..Default::default()
        };
        assert_eq!(pick_adapter(&adapters, &request), Ok(Some(1)));
    }
}

#[test]
fn pci_set_never_calls_the_uuid_exec_resolver() {
    let request = GpuRequest::from_env_with(
        |key| match key {
            GPU_UUID_ENV => Some("GPU-12345678".into()),
            GPU_PCI_ENV => Some("0000:21:00.0".into()),
            _ => None,
        },
        |_| panic!("PCI pin must never invoke the UUID/exec resolver"),
        |_, _| Err(uuid::UuidError::NotFound),
    );
    assert_eq!(request.pci.as_deref(), Some("0000:21:00.0"));
    assert!(request.uuid_pci.is_none());
}

#[cfg(unix)]
#[test]
fn uuid_only_uses_proc_fixture_directory() {
    let root = regia_test_utils::TempDir::new("gpu-uuid-proc");
    let card = root.join("0000:21:00.0");
    std::fs::create_dir(&card).expect("fixture directory");
    regia_test_utils::write_atomic(&card.join("information"), INFORMATION.as_bytes()).expect("fixture");
    let request = GpuRequest::from_env_with(
        |key| (key == GPU_UUID_ENV).then(|| "12345678".into()),
        |wanted| uuid::resolve_uuid_from_proc(wanted, &root),
        |_, _| panic!("UUID-only should use the resolver, not the cross-check"),
    );
    assert_eq!(request.uuid_pci, Some(Ok("0000:21:00.0".into())));
}

#[test]
fn parses_documented_information_layout() {
    let info = uuid::parse_information(INFORMATION).expect("documented information layout");
    assert_eq!(info.uuid, "GPU-12345678-1234-5678-9abc-123456789abc");
    assert_eq!(info.pci, "0000:21:00.0");
    assert_eq!(info.device_minor, 5);
}

#[test]
fn sleeping_fake_binary_times_out_without_a_wall_clock_assertion() {
    // The test executable is the fake binary. An already-expired deadline exercises kill
    // deterministically; no scheduler-dependent elapsed-time assertion or sync sleep.
    let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
    command.args([
        "--ignored",
        "--exact",
        "gpu_select::regression_tests::sleeping_fake_binary",
    ]);
    command.env(uuid::TEST_CHILD_ENV, "1");
    assert!(matches!(
        uuid::command_output(&mut command, std::time::Duration::ZERO),
        Err(uuid::UuidError::Timeout)
    ));
}

#[test]
#[ignore = "subprocess fixture; invoked only by sleeping_fake_binary_times_out_without_a_wall_clock_assertion"]
fn sleeping_fake_binary() {
    // `--ignored` runs this fixture by itself. Without the parent marker, return at once.
    if !uuid::test_child_invoked() {
        return;
    }
    std::thread::sleep(std::time::Duration::from_secs(60));
}

#[test]
fn proc_parser_rejects_missing_duplicate_and_malformed_fields() {
    for field in ["GPU UUID", "Bus Location", "Device Minor"] {
        let missing = INFORMATION
            .lines()
            .filter(|line| !line.starts_with(field))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            matches!(uuid::parse_information(&missing), Err(uuid::UuidError::Information(got)) if got == field)
        );
        let duplicate = format!("{INFORMATION}\n{field}: ignored\n");
        assert!(
            matches!(uuid::parse_information(&duplicate), Err(uuid::UuidError::Information(got)) if got == field)
        );
    }
    for (old, new, field) in [
        ("GPU-12345678-1234-5678-9abc-123456789abc", "GPU-nope", "GPU UUID"),
        ("0000:21:00.0", "0000:xx:00.0", "Bus Location"),
        ("0000:21:00.0", "0000:21:20.0", "Bus Location"),
        ("0000:21:00.0", "0000:21:00.8", "Bus Location"),
        ("0000:21:00.0", "0000:100:00.0", "Bus Location"),
        ("Minor:    5", "Minor:    -1", "Device Minor"),
        ("Minor:    5", "Minor:    4294967296", "Device Minor"),
    ] {
        assert!(
            matches!(uuid::parse_information(&INFORMATION.replace(old, new)), Err(uuid::UuidError::Information(got)) if got == field),
            "{new}"
        );
    }
    let whitespace = INFORMATION
        .replace('\n', "\r\n")
        .replace("GPU UUID:", " \tGPU UUID :");
    assert_eq!(
        uuid::parse_information(&whitespace).expect("whitespace"),
        uuid::parse_information(INFORMATION).expect("fixture")
    );
}

fn fixture_card(root: &std::path::Path, name: &str, text: &str) {
    let dir = root.join(name);
    std::fs::create_dir(&dir).expect("fixture card directory");
    regia_test_utils::write_atomic(&dir.join("information"), text.as_bytes()).expect("atomic fixture");
}

#[test]
fn proc_reader_reports_missing_empty_malformed_and_unreadable_entries() {
    let root = regia_test_utils::TempDir::new("uuid-proc-errors");
    assert!(
        matches!(uuid::resolve_uuid_from_proc("1234", &root.join("missing")), Err(uuid::UuidError::ProcIo { source, .. }) if source.kind() == std::io::ErrorKind::NotFound)
    );
    assert!(matches!(
        uuid::resolve_uuid_from_proc("1234", &root),
        Err(uuid::UuidError::NotFound)
    ));
    fixture_card(&root, "bad", "GPU UUID: missing everything else");
    match uuid::resolve_uuid_from_proc("1234", &root) {
        Err(uuid::UuidError::CardInformation { field, card }) => {
            assert_eq!(field, "GPU UUID");
            assert_eq!(card, "bad");
        }
        other => panic!("malformed card must be named in the error, got {other:?}"),
    }
    std::fs::remove_file(root.join("bad/information")).expect("remove file");
    // A directory in place of the information file fails even as root (chmod would not).
    std::fs::create_dir(root.join("bad/information")).expect("unreadable fixture");
    match uuid::resolve_uuid_from_proc("1234", &root) {
        Err(uuid::UuidError::ProcIo { card, .. }) => assert_eq!(card, "bad"),
        other => panic!("unreadable card must be named in the error, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn proc_reader_requires_a_unique_case_insensitive_uuid_prefix() {
    let root = regia_test_utils::TempDir::new("uuid-prefix");
    fixture_card(&root, "0000:21:00.0", INFORMATION);
    let other = INFORMATION
        .replace("12345678-1234", "12345679-1234")
        .replace("0000:21:00.0", "0000:01:00.0");
    fixture_card(&root, "0000:01:00.0", &other);
    for wanted in [
        "GPU-12345678",
        "12345678",
        "gpu-12345678-1234-5678-9ABC-123456789ABC",
    ] {
        assert_eq!(
            uuid::resolve_uuid_from_proc(wanted, &root).expect("unique prefix"),
            "0000:21:00.0"
        );
    }
    assert!(matches!(
        uuid::resolve_uuid_from_proc("GPU-1234567", &root),
        Err(uuid::UuidError::Ambiguous)
    ));
    for wanted in ["", "GPU-", "GPU-ffffffff", "invalid"] {
        assert!(matches!(
            uuid::resolve_uuid_from_proc(wanted, &root),
            Err(uuid::UuidError::NotFound)
        ));
    }
    // A corrupt second card must not turn an ambiguous prefix into a false unique match.
    regia_test_utils::write_atomic(&root.join("0000:01:00.0/information"), b"broken").expect("fixture");
    match uuid::resolve_uuid_from_proc("1234567", &root) {
        Err(uuid::UuidError::CardInformation { card, .. }) => assert_eq!(card, "0000:01:00.0"),
        other => panic!("short prefix must still fail and name the corrupt card, got {other:?}"),
    }
}

#[test]
fn pci_cross_check_preserves_mismatch_and_ignores_unavailable_procfs() {
    for resolved in [Ok("0000:01:00.0".into()), Err(uuid::UuidError::NotFound)] {
        let request = GpuRequest::from_env_with(
            |key| match key {
                GPU_UUID_ENV => Some(" GPU-12345678 ".into()),
                GPU_PCI_ENV => Some(" 0000:21:00.0 ".into()),
                GPU_STRICT_ENV => Some("1".into()),
                _ => None,
            },
            |_| panic!("full UUID resolver is forbidden with PCI"),
            |uuid, pci| {
                assert_eq!(uuid, "GPU-12345678");
                assert_eq!(pci, "0000:21:00.0");
                resolved
            },
        );
        assert_eq!(
            pick_adapter(&[("pinned".into(), "0000:21:00.0".into())], &request),
            Ok(Some(0))
        );
    }
}

#[test]
fn empty_pci_uses_uuid_and_unresolved_uuid_remains_an_error() {
    let request = GpuRequest::from_env_with(
        |key| match key {
            GPU_PCI_ENV => Some(" \t ".into()),
            GPU_UUID_ENV => Some("GPU-missing".into()),
            _ => None,
        },
        |_| Err(uuid::UuidError::NotFound),
        |_, _| panic!("no PCI cross-check"),
    );
    assert!(request.pci.is_none());
    assert!(
        pick_adapter(&[], &request)
            .expect_err("failed UUID lookup")
            .contains("UUID is not listed")
    );
}

#[test]
fn no_uuid_does_not_touch_procfs_or_the_resolver() {
    for pci in [None, Some("0000:21:00.0".to_owned())] {
        let request = GpuRequest::from_env_with(
            |key| if key == GPU_PCI_ENV { pci.clone() } else { None },
            |_| panic!("no UUID resolver"),
            |_, _| panic!("no procfs access"),
        );
        assert_eq!(request.pci, pci);
        assert!(request.uuid_pci.is_none());
    }
}

#[test]
fn nonzero_deadline_also_kills_the_sleeping_fake_binary() {
    let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
    command.args([
        "--ignored",
        "--exact",
        "gpu_select::regression_tests::sleeping_fake_binary",
    ]);
    command.env(uuid::TEST_CHILD_ENV, "1");
    // This is an operation deadline, not an elapsed-time performance assertion or a sync sleep.
    assert!(matches!(
        uuid::command_output(&mut command, std::time::Duration::from_millis(25)),
        Err(uuid::UuidError::Timeout)
    ));
}

#[test]
fn conflicting_cross_check_warns_but_equivalent_pci_spellings_do_not() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tracing_subscriber::prelude::*;
    struct Warnings(Arc<AtomicUsize>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Warnings {
        fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    let warnings = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(Warnings(warnings.clone()));
    tracing::subscriber::with_default(subscriber, || {
        let adapters = [("pinned".into(), "0000:21:00.0".into())];
        let mut request = GpuRequest {
            pci: Some("0000:21:00.0".into()),
            uuid_pci: Some(Ok("0000:01:00.0".into())),
            ..Default::default()
        };
        assert_eq!(pick_adapter(&adapters, &request), Ok(Some(0)));
        assert_eq!(warnings.load(Ordering::Relaxed), 1);
        request.uuid_pci = Some(Ok("00000000:21:00.0".into()));
        assert_eq!(pick_adapter(&adapters, &request), Ok(Some(0)));
        assert_eq!(warnings.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn invalid_explicit_pci_does_not_silently_select_the_uuid_card() {
    let request = GpuRequest {
        pci: Some("invalid".into()),
        uuid_pci: Some(Ok("0000:21:00.0".into())),
        ..Default::default()
    };
    assert!(
        pick_adapter(&[("UUID card".into(), "0000:21:00.0".into())], &request)
            .expect_err("invalid PCI")
            .contains(GPU_PCI_ENV)
    );
}

#[test]
fn proc_parser_handles_truncation_and_arbitrary_utf8_without_panicking() {
    for end in 0..INFORMATION.len() {
        let _ = uuid::parse_information(&INFORMATION[..end]);
    }
    for text in [
        "\0\0",
        "GPU UUID: 🦀\nBus Location: λ\nDevice Minor: ∞",
        "GPU UUID:\nBus Location:\nDevice Minor:",
        "GPU UUID: GPU-\n\0",
    ] {
        assert!(uuid::parse_information(text).is_err());
    }
}

const FULL_UUID: &str = "GPU-12345678-1234-5678-9abc-123456789abc";

fn uninitialized_card_text() -> String {
    INFORMATION
        .replace(FULL_UUID, "GPU-????????-????-????-????-????????????")
        .replace("0000:21:00.0", "0000:01:00.0")
        .replace("Minor:    5", "Minor:    1")
}

#[test]
fn question_mark_uuid_is_uninitialized_not_a_generic_field_error() {
    assert!(matches!(
        uuid::parse_information(&uninitialized_card_text()),
        Err(uuid::UuidError::UninitializedUuid)
    ));
    assert!(matches!(
        uuid::parse_information("GPU UUID: ????\n"),
        Err(uuid::UuidError::UninitializedUuid)
    ));
}

#[cfg(unix)]
#[test]
fn exact_full_uuid_survives_malformed_unreadable_and_uninitialized_neighbors() {
    // At 3e113ff0f each of these returned an error for the neighbor and dropped the match.
    for (label, plant) in [
        ("malformed", Plant::Text("not an information file")),
        ("unreadable", Plant::Directory),
        ("uninitialized", Plant::TextOwned),
    ] {
        let root = regia_test_utils::TempDir::new("uuid-exact-neighbor");
        fixture_card(&root, "0000:21:00.0", INFORMATION);
        let neighbor = root.join("0000:01:00.0");
        std::fs::create_dir(&neighbor).expect("neighbor card");
        match plant {
            Plant::Text(text) => {
                regia_test_utils::write_atomic(&neighbor.join("information"), text.as_bytes())
                    .expect("neighbor");
            }
            Plant::TextOwned => {
                regia_test_utils::write_atomic(
                    &neighbor.join("information"),
                    uninitialized_card_text().as_bytes(),
                )
                .expect("neighbor");
            }
            Plant::Directory => {
                std::fs::create_dir(neighbor.join("information")).expect("unreadable information");
            }
        }
        assert_eq!(
            uuid::resolve_uuid_from_proc(FULL_UUID, &root).unwrap_or_else(|err| panic!("{label}: {err}")),
            "0000:21:00.0",
            "{label}"
        );
    }
}

#[cfg(unix)]
enum Plant {
    Text(&'static str),
    TextOwned,
    Directory,
}

#[cfg(unix)]
#[test]
fn short_prefix_names_the_uninitialized_neighbor() {
    let root = regia_test_utils::TempDir::new("uuid-uninit-prefix");
    fixture_card(&root, "0000:21:00.0", INFORMATION);
    fixture_card(&root, "0000:01:00.0", &uninitialized_card_text());
    let err = uuid::resolve_uuid_from_proc("12345678", &root).expect_err("prefix must fail closed");
    let msg = err.to_string();
    assert!(msg.contains("0000:01:00.0"), "{msg}");
    assert!(msg.contains("persistence mode"), "{msg}");
    assert!(msg.contains("REGIA_GPU_PCI"), "{msg}");
}

#[cfg(unix)]
#[test]
fn uninitialized_card_error_names_that_card() {
    let root = regia_test_utils::TempDir::new("uuid-uninit-only");
    let text = uninitialized_card_text().replace("0000:01:00.0", "0000:aa:00.0");
    fixture_card(&root, "0000:aa:00.0", &text);
    let err = uuid::resolve_uuid_from_proc(FULL_UUID, &root).expect_err("uninitialized card");
    let msg = err.to_string();
    assert!(msg.contains("0000:aa:00.0"), "{msg}");
    assert!(msg.contains("persistence mode"), "{msg}");
    assert!(msg.contains("REGIA_GPU_PCI"), "{msg}");
}

#[cfg(unix)]
#[test]
fn pci_pin_reads_only_the_pinned_card() {
    let root = regia_test_utils::TempDir::new("uuid-pci-one");
    fixture_card(&root, "0000:21:00.0", INFORMATION);
    std::fs::create_dir_all(root.join("0000:01:00.0/information")).expect("unreadable neighbor");
    let prefix = "12345678";
    assert!(
        uuid::resolve_uuid_from_proc(prefix, &root).is_err(),
        "a short prefix must still fail closed when a neighbor cannot be read"
    );
    assert_eq!(
        uuid::cross_check_pinned(prefix, "0000:21:00.0", &root).expect("pinned file"),
        "0000:21:00.0"
    );
    assert_eq!(
        uuid::cross_check_pinned(prefix, "00000000:21:00.0", &root).expect("eight-digit domain"),
        "0000:21:00.0"
    );
    let request = GpuRequest::from_env_with(
        |key| match key {
            GPU_UUID_ENV => Some(prefix.into()),
            GPU_PCI_ENV => Some("00000000:21:00.0".into()),
            _ => None,
        },
        |_| panic!("PCI pin must not resolve every card or run nvidia-smi"),
        |uuid, pci| uuid::cross_check_pinned(uuid, pci, &root),
    );
    assert_eq!(request.uuid_pci, Some(Ok("0000:21:00.0".into())));
    assert_eq!(
        pick_adapter(&[("pinned".into(), "00000000:21:00.0".into())], &request),
        Ok(Some(0))
    );
}

#[cfg(unix)]
#[test]
fn pinned_cross_check_warns_on_disagreement_without_reading_a_neighbor() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tracing_subscriber::prelude::*;
    struct Warnings(Arc<AtomicUsize>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Warnings {
        fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    let warnings = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(Warnings(warnings.clone()));
    tracing::subscriber::with_default(subscriber, || {
        let root = regia_test_utils::TempDir::new("uuid-pci-mismatch");
        fixture_card(&root, "0000:21:00.0", INFORMATION);
        std::fs::create_dir_all(root.join("0000:01:00.0/information")).expect("unreadable neighbor");
        assert_eq!(
            uuid::cross_check_pinned(FULL_UUID, "0000:21:00.0", &root).expect("match"),
            "0000:21:00.0"
        );
        assert_eq!(warnings.load(Ordering::Relaxed), 0);
        let other = INFORMATION.replace("12345678-1234", "aaaaaaaa-1234");
        regia_test_utils::write_atomic(&root.join("0000:21:00.0/information"), other.as_bytes())
            .expect("swap");
        // A directory-wide read would fail on the unreadable neighbor instead of NotFound.
        assert!(matches!(
            uuid::cross_check_pinned(FULL_UUID, "0000:21:00.0", &root),
            Err(uuid::UuidError::NotFound)
        ));
        assert_eq!(warnings.load(Ordering::Relaxed), 1);
    });
}

#[cfg(unix)]
#[test]
fn missing_procfs_uses_the_injected_fallback_and_a_present_tree_never_does() {
    let scratch = regia_test_utils::TempDir::new("uuid-smi-gate");
    let missing = scratch.join("absent-nvidia-proc");
    let mut calls = 0u32;
    let pci = uuid::resolve_linux(FULL_UUID, &missing, |_| {
        calls += 1;
        Ok("00000000:21:00.0".into())
    })
    .expect("fallback result");
    assert_eq!(calls, 1);
    assert_eq!(pci, "00000000:21:00.0");

    std::fs::create_dir(scratch.join("gpus")).expect("gpus");
    let err = uuid::resolve_linux(FULL_UUID, &scratch, |_| {
        panic!("nvidia-smi must not run when /proc/driver/nvidia exists");
    });
    assert!(matches!(err, Err(uuid::UuidError::NotFound)), "{err:?}");

    let file = scratch.join("nvidia-file");
    std::fs::write(&file, b"x").expect("file");
    let err = uuid::resolve_linux(FULL_UUID, &file, |_| {
        panic!("a non-directory proc path must not run nvidia-smi");
    });
    assert!(matches!(err, Err(uuid::UuidError::ProcIo { .. })), "{err:?}");

    let blocked = file.join("child");
    let err = uuid::resolve_linux(FULL_UUID, &blocked, |_| {
        panic!("an unstatable proc path must not run nvidia-smi");
    });
    assert!(matches!(err, Err(uuid::UuidError::ProcIo { .. })), "{err:?}");

    let good = scratch.join("gpus/0000:21:00.0");
    std::fs::create_dir_all(&good).expect("card");
    regia_test_utils::write_atomic(&good.join("information"), INFORMATION.as_bytes()).expect("info");
    std::fs::create_dir_all(scratch.join("gpus/0000:01:00.0/information")).expect("unreadable neighbor");
    let pci = uuid::resolve_linux(FULL_UUID, &scratch, |_| {
        panic!("a readable exact UUID must not run nvidia-smi");
    })
    .expect("exact match");
    assert_eq!(pci, "0000:21:00.0");
}

#[test]
fn ignored_child_fixtures_return_without_the_parent_marker() {
    // Exit status is the assertion. The deadline is only a hang guard for a fixture that slept.
    for name in [
        "gpu_select::regression_tests::sleeping_fake_binary",
        "gpu_select::uuid::output_tests::fake_output_binary",
    ] {
        let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
        command.args(["--ignored", "--exact", "--nocapture", name]);
        command.env_remove(uuid::TEST_CHILD_ENV);
        command.env_remove("REGIA_GPU_UUID_TEST_OUTPUT");
        uuid::command_output(&mut command, std::time::Duration::from_secs(10))
            .unwrap_or_else(|err| panic!("{name} without the parent marker: {err}"));
    }
}
