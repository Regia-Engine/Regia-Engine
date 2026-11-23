// SPDX-License-Identifier: MPL-2.0
use std::path::{Path, PathBuf};

use super::*;

type Result = anyhow::Result<()>;

struct Tree(PathBuf);
impl Tree {
    fn new() -> Self {
        Self(regia_test_utils::unique_temp_dir("package-hygiene"))
    }
    fn write(&self, path: &str, data: &[u8]) -> Result {
        let path = self.0.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        regia_test_utils::write_atomic(&path, data)?;
        Ok(())
    }
    fn root(&self) -> &Path {
        &self.0
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn planted_host_string_refuses_project_package_by_default() -> Result {
    let project = Tree::new();
    let staging = Tree::new();
    project.write("Regia.toml", b"")?;
    staging.write(
        "player",
        format!("\x7fELF\0owner bench: {}\0", private_name(0)).as_bytes(),
    )?;
    let error = check_project_tree(project.root(), staging.root()).expect_err("host leak rejected");
    assert_eq!(error.code, "package_hygiene_failed");
    let report = error.report.expect("completed report");
    assert!(report.complete);
    assert!(report.findings.iter().any(|hit| hit.rule == "host_string"));
    assert!(!error.location.contains(&private_name(0)));
    Ok(())
}

#[test]
fn host_string_allowance_is_exact_and_other_strings_still_fail() -> Result {
    let project = Tree::new();
    let staging = Tree::new();
    project.write(
        "Regia.toml",
        b"[package]\nhost_strings_allow = 'host-allow.toml'\n",
    )?;
    project.write(
        "host-allow.toml",
        b"[[allow]]\npath = 'player.exe'\ntext = 'public example /srv/content'\nreason = 'Public documentation example'\n",
    )?;
    staging.write("player.exe", b"MZ\0public example /srv/content\0")?;
    let report = check_project_tree(project.root(), staging.root())?;
    assert_eq!(report.allowed_findings, 1);
    staging.write(
        "player.exe",
        format!("MZ\0public example /srv/content\0built on {}\0", private_name(1)).as_bytes(),
    )?;
    assert!(check_project_tree(project.root(), staging.root()).is_err());
    staging.write("player.exe", b"MZ\0public example /srv/content-private\0")?;
    assert!(check_project_tree(project.root(), staging.root()).is_err());
    Ok(())
}

#[test]
fn missing_host_string_allow_file_fails_closed() -> Result {
    let project = Tree::new();
    let staging = Tree::new();
    project.write("Regia.toml", b"[package]\nhost_strings_allow = 'missing.toml'\n")?;
    staging.write("player", b"\x7fELF\0clean binary\0")?;
    let error = check_project_tree(project.root(), staging.root()).expect_err("missing allow file");
    assert_eq!(error.code, "package_hygiene_io");
    Ok(())
}

/// Every executable format the stage can hold is scanned; data files are not executables.
#[test]
fn host_string_gate_scans_every_executable_format_and_skips_data_files() -> Result {
    let project = Tree::new();
    let staging = Tree::new();
    project.write("Regia.toml", b"")?;
    staging.write("game", b"\x7fELF\0owner bench, 2026-10-11 arc-sweep\0")?;
    staging.write(
        "libmix.dylib",
        &[
            b"\xcf\xfa\xed\xfe\0".as_slice(),
            format!("built on {}\0", private_name(1)).as_bytes(),
        ]
        .concat(),
    )?;
    staging.write(
        "player.wasm",
        &[
            b"\0asm\x01\0\0\0".as_slice(),
            format!("{} bench\0", private_name(0)).as_bytes(),
        ]
        .concat(),
    )?;
    staging.write(
        "notes.txt",
        format!("built on {}: a text file with the same words\0", private_name(1)).as_bytes(),
    )?;
    let error = check_project_tree(project.root(), staging.root()).expect_err("two executables leak");
    assert_eq!(error.code, "package_hygiene_failed");
    let report = error.report.expect("completed report");
    let hosts: Vec<&Finding> = report
        .findings
        .iter()
        .filter(|hit| hit.rule == "host_string")
        .collect();
    assert_eq!(hosts.len(), 2, "{:?}", report.findings);
    assert!(report.complete);
    Ok(())
}

/// The private names unmasked at test time, so this file never spells them out.
fn private_names() -> Vec<String> {
    super::host_strings::PRIVATE
        .iter()
        .map(|(name, _)| name.iter().map(|byte| char::from(byte ^ 0xA5)).collect())
        .collect()
}

/// One private name unmasked at test time (its row in `PRIVATE`).
fn private_name(row: usize) -> String {
    private_names().swap_remove(row)
}

/// ADR-10354: the gate's own private names never exist as one printable string in the tool.
#[test]
fn private_names_are_masked_in_the_tool_and_match_case_insensitively() {
    let source = super::host_strings::pattern_source();
    let names = private_names();
    assert_eq!(names.len(), 8);
    for name in &names {
        assert!(source.contains(name.as_str()), "{source}");
    }
    for generic in [
        "/media/",
        "/srv/",
        "/models/",
        "/home/",
        r"192\.168\.",
        r"\.ts\.net",
        "tailscale",
    ] {
        assert!(source.contains(generic), "{source}");
    }
    // No masked byte is printable, so `strings` cannot join two rows into a name.
    assert!(
        super::host_strings::PRIVATE
            .iter()
            .flat_map(|(name, _)| name.iter())
            .all(|byte| !byte.is_ascii_graphic())
    );
    let scanner = super::host_strings::Scanner::new(None).expect("the pattern compiles");
    for text in [
        &b"/home/ann/game/src/main.rs"[..],
        b"http://192.168.1.1:5000/desc.xml",
        b"host.station.ts.net",
        b"tailscaled is running",
        b"built by PiercingXX",
    ] {
        assert!(
            scanner.pattern.is_match(text),
            "{:?} must match",
            String::from_utf8_lossy(text)
        );
    }
    // Every private name matches in a sentence and in upper case, without spelling it here.
    for name in &names {
        for text in [
            format!("host {name}.local"),
            format!("built on {}", name.to_uppercase()),
        ] {
            assert!(
                scanner.pattern.is_match(text.as_bytes()),
                "a private name must match"
            );
        }
    }
    // A PE stores the channel labels "left main" and "right main" back to back; `strings` prints
    // `mainright`, which names no host, so the plain compound matches at token boundaries only.
    for clean in [
        &b"owner bench, 2026-10-11 arc-sweep"[..],
        b"/builder/cargo/registry/src/lib.rs",
        b"10.0.0.1:5000",
        b"left mainright maininverse mass",
    ] {
        assert!(
            !scanner.pattern.is_match(clean),
            "{:?}",
            String::from_utf8_lossy(clean)
        );
    }
}

#[test]
fn planted_dev_flag_string_fails_package() -> Result {
    let root = regia_test_utils::unique_temp_dir("package-hygiene-planted-flag");
    regia_test_utils::write_atomic(&root.join("player"), b"\x7fELF\0merged--developer-consoleNEXT")?;
    let policy = Policy {
        forbidden_strings: vec!["--developer-console".into()],
        ..Policy::default()
    };
    let result = check_tree(&root, &policy);
    std::fs::remove_dir_all(root)?;
    assert!(result.is_err(), "a planted dev flag must refuse the package");
    let error = result.expect_err("rejected");
    assert_eq!(error.code, "package_hygiene_failed");
    let report = error.report.expect("completed report");
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].rule, "forbidden_string_1");
    assert_eq!(report.findings[0].offset, 11);
    Ok(())
}

#[test]
fn literals_are_literal_and_case_sensitive_and_regex_is_explicit() -> Result {
    let tree = Tree::new();
    tree.write("player", b"\0aXb DEVELOPMENT FROB_DEBUG7\xff")?;
    let policy = Policy {
        forbidden_strings: vec!["a.b".into(), "development".into()],
        forbidden_patterns: vec![r"FROB_DEBUG[0-9]+".into()],
        ..Policy::default()
    };
    let report = audit_tree(tree.root(), &policy)?;
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].rule, "forbidden_pattern_1");
    Ok(())
}

#[test]
fn deny_term_kinds_names_and_binary_offsets() -> Result {
    let tree = Tree::new();
    tree.write(
        "private-folder/ok.txt",
        "sEcReT secrets secret_key\nReview\n".as_bytes(),
    )?;
    tree.write("binary", b"\xffSECRET\xfe")?;
    let policy = Policy {
        deny_terms: vec![
            "word:secret".into(),
            "sub:private-folder".into(),
            "re:Rev[iI]ew".into(),
        ],
        ..Policy::default()
    };
    let report = audit_tree(tree.root(), &policy)?;
    assert_eq!(report.findings.len(), 5);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.path == "binary" && f.offset == 1 && f.rule == "deny_term_1")
    );
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.source == "name" && f.rule == "deny_term_2" && f.path == "[redacted]")
    );
    tree.write("private-folder/ok.txt", b"secrets secret_key")?;
    let report = audit_tree(tree.root(), &policy)?;
    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.path != "binary" && f.rule == "deny_term_1")
    );
    Ok(())
}

#[test]
fn secrets_in_binary_and_text_are_redacted() -> Result {
    let tree = Tree::new();
    // Synthetic markers assembled here so no real credential or private data is a fixture.
    let samples = [
        (
            "secret_private_key",
            format!("-----BEGIN {} PRIVATE KEY-----", "RSA"),
        ),
        (
            "secret_webhook",
            ["https://discord.com/api/", "webhooks/123456/synthetic"].concat(),
        ),
        (
            "secret_webhook",
            ["https://hooks.slack.com/", "services/TTEST/BTEST/synthetic"].concat(),
        ),
        ("secret_aws_access_key", format!("AKIA{}", "A".repeat(16))),
        ("secret_github_token", format!("ghp_{}", "a".repeat(36))),
        ("secret_github_token", format!("github_pat_{}", "a".repeat(25))),
        ("secret_api_key", format!("sk-{}", "a".repeat(24))),
        ("secret_slack_token", format!("xoxb-{}", "a".repeat(16))),
        (
            "secret_bot_token",
            format!("Bot {}.{}.synthetic", "a".repeat(24), "b".repeat(6)),
        ),
    ];
    for (i, (rule, sample)) in samples.iter().enumerate() {
        tree.write(
            &format!("fixture-{i}"),
            format!("\0prefix {sample} suffix\0").as_bytes(),
        )?;
        let report = audit_tree(tree.root(), &Policy::default())?;
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.path == format!("fixture-{i}") && f.rule == *rule),
            "{rule}"
        );
        let json = serde_json::to_string(&report)?;
        assert!(!json.contains(sample), "report exposed matched bytes");
        let error = check_tree(tree.root(), &Policy::default()).expect_err("secret refused");
        assert!(!error.to_string().contains(sample));
    }
    Ok(())
}

#[test]
fn reviewed_allowance_only_waives_its_path_and_rule() -> Result {
    let tree = Tree::new();
    tree.write("licenses/CREDITS.txt", b"contact@example.invalid\nreserved")?;
    tree.write("settings.txt", b"contact@example.invalid")?;
    let policy = Policy {
        scan_emails: true,
        deny_terms: vec!["reserved".into()],
        allow: vec![Allowance {
            rule: "secret_email".into(),
            path: "licenses/*.txt".into(),
            reason: "Public attribution contact in reviewed licenses".into(),
            definition: None,
        }],
        ..Policy::default()
    };
    let report = audit_tree(tree.root(), &policy)?;
    assert_eq!(report.allowed_findings, 1);
    assert_eq!(report.findings.len(), 2);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.path == "settings.txt" && f.rule == "secret_email")
    );
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.path == "licenses/CREDITS.txt" && f.rule == "deny_term_1")
    );
    Ok(())
}

#[test]
fn glob_semantics_cover_basename_recursive_and_exact_paths() -> Result {
    let tree = Tree::new();
    for path in ["a.txt", "nested/a.txt", "nested/deep/a.txt", "nested/a.bin"] {
        tree.write(path, b"private")?;
    }
    for (glob, allowed) in [
        ("*.txt", 3),
        ("**/*.txt", 3),
        ("nested/*.txt", 1),
        ("nested/**/*.txt", 2),
        ("nested/a.?in", 1),
    ] {
        let policy = Policy {
            deny_terms: vec!["private".into()],
            allow: vec![Allowance {
                rule: "deny_term_1".into(),
                path: glob.into(),
                reason: "Reviewed fixture".into(),
                definition: None,
            }],
            ..Policy::default()
        };
        let report = audit_tree(tree.root(), &policy)?;
        assert_eq!(report.allowed_findings, allowed, "{glob}");
    }
    Ok(())
}

#[test]
fn invalid_policy_fails_without_echoing_patterns() {
    for text in [
        "[package_hygiene]\nforbidden_strings = ['']",
        "[package_hygiene]\nforbidden_patterns = ['sensitive[']",
        "[package_hygiene]\ndeny_terms = ['word:']",
        "[package_hygiene]\ndeny_terms = ['re:(?=sensitive)']",
        "[package_hygiene]\nmax_file_bytes = 0",
        "[package_hygiene]\nscan_secerts = false",
        "[package_hygiene]\nforbidden_strings = 'sensitive'",
        "[package_hygiene]\nforbidden_patterns = ['.*']",
        "[package_hygiene]\nallow = [{rule='secret_email', path='*', reason='reviewed'}]",
        "[package_hygiene]\nallow = [{rule='secret_private_key', path='*', reason=' '}]",
        "[package_hygiene]\nallow = [{rule='secret_private_key', path='../*', reason='reviewed'}]",
        "[package_hygiene]\nallow = [{rule='secret_private_key', path='C:/temp/*', reason='reviewed'}]",
        "[[package_hygiene.rules]]\nid='same'\nkind='string'\nbody='one'\n[[package_hygiene.rules]]\nid='same'\nkind='string'\nbody='two'\n",
        "[[package_hygiene.rules]]\nid='named'\nkind='string'\nbody='kept'\n[[package_hygiene.allow]]\nrule='named'\npath='*'\nreason=' '\n",
    ] {
        let error = Policy::from_manifest(text).expect_err("invalid policy");
        assert_eq!(error.code, "package_hygiene_config");
        assert!(!error.to_string().contains("sensitive"));
    }
}

#[test]
fn manifest_default_and_custom_policy_round_trip() -> Result {
    assert_eq!(
        Policy::from_manifest("[project]\nname='Sample'\n")?,
        Policy::default()
    );
    let policy = Policy::from_manifest(
        "[package_hygiene]\nforbidden_strings=['--developer-console']\nscan_emails=true\nmax_file_bytes=4096\n",
    )?;
    assert_eq!(policy.forbidden_strings, ["--developer-console"]);
    assert!(policy.rules.is_empty());
    assert!(policy.scan_emails);
    assert_eq!(policy.max_file_bytes, 4096);
    let named = Policy::from_manifest(
        "[[package_hygiene.rules]]\nid='dev_console'\nkind='string'\nbody='--developer-console'\n",
    )?;
    assert_eq!(named.rules.len(), 1);
    assert_eq!(named.rules[0].id, "dev_console");
    assert_eq!(named.migrated()?, named);
    Ok(())
}

#[test]
fn ignored_files_are_still_audited_and_order_is_stable() -> Result {
    let tree = Tree::new();
    tree.write(".gitignore", b"ignored/\n")?;
    tree.write("z.bin", b"--developer-console")?;
    tree.write("ignored/a.bin", b"--developer-console")?;
    let policy = Policy {
        forbidden_strings: vec!["--developer-console".into()],
        ..Policy::default()
    };
    let first = audit_tree(tree.root(), &policy)?;
    assert_eq!(first.findings.len(), 2);
    assert_eq!(first.findings[0].path, "ignored/a.bin");
    assert_eq!(first, audit_tree(tree.root(), &policy)?);
    Ok(())
}

#[test]
fn size_entry_and_finding_limits_fail_closed() -> Result {
    let tree = Tree::new();
    tree.write("a", b"private")?;
    tree.write("b", b"private")?;
    let policies = [
        Policy {
            max_file_bytes: 6,
            ..Policy::default()
        },
        Policy {
            max_total_bytes: 7,
            ..Policy::default()
        },
        Policy {
            max_entries: 1,
            ..Policy::default()
        },
        Policy {
            deny_terms: vec!["private".into()],
            max_findings: 1,
            ..Policy::default()
        },
    ];
    for policy in policies {
        assert_eq!(
            audit_tree(tree.root(), &policy)
                .expect_err("incomplete must fail")
                .code,
            "package_hygiene_limit"
        );
    }
    let exact = Policy {
        max_file_bytes: 7,
        max_total_bytes: 14,
        max_entries: 2,
        ..Policy::default()
    };
    assert!(check_tree(tree.root(), &exact).is_ok());
    Ok(())
}

#[test]
fn missing_root_and_corrupt_pak_never_pass() -> Result {
    let tree = Tree::new();
    assert_eq!(
        audit_tree(&tree.root().join("absent"), &Policy::default())
            .expect_err("missing")
            .code,
        "package_hygiene_io"
    );
    tree.write("bad.fpak", b"not a pak")?;
    assert_eq!(
        audit_tree(tree.root(), &Policy::default())
            .expect_err("corrupt")
            .code,
        "package_hygiene_pak"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn links_never_escape_or_hide_data() -> Result {
    let outside = Tree::new();
    outside.write("private", b"do not open outside")?;
    let tree = Tree::new();
    std::os::unix::fs::symlink(outside.root(), tree.root().join("link"))?;
    assert_eq!(
        audit_tree(tree.root(), &Policy::default())
            .expect_err("link")
            .code,
        "package_hygiene_path"
    );
    std::fs::remove_file(tree.root().join("link"))?;
    std::os::unix::fs::symlink(outside.root().join("absent"), tree.root().join("link"))?;
    assert_eq!(
        audit_tree(tree.root(), &Policy::default())
            .expect_err("dangling link")
            .code,
        "package_hygiene_path"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn non_utf8_and_control_names_are_refused_without_lossy_aliases() -> Result {
    use std::os::unix::ffi::OsStringExt;
    let tree = Tree::new();
    let path = tree.root().join(std::ffi::OsString::from_vec(vec![0xff]));
    regia_test_utils::write_atomic(&path, b"x")?;
    assert_eq!(
        audit_tree(tree.root(), &Policy::default())
            .expect_err("bad name")
            .code,
        "package_hygiene_path"
    );
    std::fs::remove_file(path)?;
    tree.write("forged\nline", b"x")?;
    let err = audit_tree(tree.root(), &Policy::default()).expect_err("bad name");
    assert!(!err.to_string().contains('\n'));
    Ok(())
}

#[test]
fn compressed_pak_payload_cannot_hide_a_forbidden_string() -> Result {
    use regia_asset::{AssetId, AssetManifest};
    use regia_asset_pipeline::pak_writer::{PakWriteOptions, write_pak};
    use regia_core::id::TypePathHash;

    let cooked = Tree::new();
    let staged = Tree::new();
    let id = AssetId::NIL;
    // Vary the prefix enough to stay below the reader's decompression-bomb ratio cap;
    // the writer deliberately stores extremely repetitive payloads without compression.
    let mut payload = Vec::new();
    for first in b'A'..=b'Z' {
        for second in b'A'..=b'Z' {
            payload.extend_from_slice(&[first, second, b'-']);
        }
    }
    payload.extend_from_slice(&b"--developer-console".repeat(500));
    cooked.write(&regia_asset::cooked_file_name(id), &payload)?;
    write_pak(
        &staged.root().join("base.fpak"),
        cooked.root(),
        &[(id, TypePathHash(0))],
        &AssetManifest::default(),
        &PakWriteOptions::default(),
    )?;
    let policy = Policy {
        forbidden_strings: vec![String::from_utf8(payload.clone())?],
        ..Policy::default()
    };
    let encoded = std::fs::read(staged.root().join("base.fpak"))?;
    let pak = regia_asset::pak::PakReader::from_bytes(encoded.clone(), "fixture", encoded.len() as u64)?;
    let entry = pak.entry(id).expect("fixture entry");
    assert!(entry.compressed_size < entry.size, "fixture must use compression");
    assert!(
        !encoded.windows(payload.len()).any(|w| w == payload),
        "fixture must hide the full literal by compression"
    );
    let report = audit_tree(staged.root(), &policy)?;
    assert_eq!(report.pak_entries_scanned, 1);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.path.starts_with("base.fpak::") && f.rule == "forbidden_string_1")
    );
    let limited = Policy {
        max_file_bytes: encoded.len() as u64,
        ..policy
    };
    assert_eq!(
        audit_tree(staged.root(), &limited).expect_err("decoded cap").code,
        "package_hygiene_limit"
    );
    Ok(())
}

#[test]
fn untyped_private_key_files_are_rejected_even_with_term_name_scanning_off() -> Result {
    let tree = Tree::new();
    tree.write("keys/dev.key", &[7; 32])?;
    tree.write("keys/id_ed25519.pub", b"public")?;
    let policy = Policy {
        scan_names: false,
        ..Policy::default()
    };
    let report = audit_tree(tree.root(), &policy)?;
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].rule, "secret_key_file");
    assert_eq!(report.findings[0].path, "keys/dev.key");
    Ok(())
}

#[test]
fn optional_secret_and_email_checks_have_explicit_defaults() -> Result {
    let tree = Tree::new();
    tree.write("credits.txt", b"contact@example.invalid")?;
    assert!(check_tree(tree.root(), &Policy::default()).is_ok());
    let email = Policy {
        scan_emails: true,
        ..Policy::default()
    };
    assert_eq!(audit_tree(tree.root(), &email)?.findings[0].rule, "secret_email");
    tree.write("key.txt", b"-----BEGIN PRIVATE KEY-----")?;
    assert!(check_tree(tree.root(), &Policy::default()).is_err());
    assert!(
        check_tree(
            tree.root(),
            &Policy {
                scan_secrets: false,
                ..Policy::default()
            }
        )
        .is_ok()
    );
    Ok(())
}

#[test]
fn policy_size_and_rule_count_caps_are_errors() {
    assert!(Policy::from_manifest(&" ".repeat((1 << 20) + 1)).is_err());
    let policy = Policy {
        forbidden_strings: vec!["x".into(); 1025],
        ..Policy::default()
    };
    assert!(policy.validate().is_err());
    let policy = Policy {
        forbidden_strings: vec!["x".repeat(16_385)],
        ..Policy::default()
    };
    assert!(policy.validate().is_err());
}

#[test]
fn pak_manifest_names_are_data_and_report_the_asset_id() -> Result {
    use regia_asset::{AssetId, AssetManifest, ManifestEntry};
    use regia_asset_pipeline::pak_writer::{PakWriteOptions, write_pak};
    use regia_core::id::TypePathHash;
    let cooked = Tree::new();
    let staged = Tree::new();
    let id = AssetId::NIL;
    cooked.write(&regia_asset::cooked_file_name(id), b"clean payload")?;
    let manifest = AssetManifest {
        entries: vec![ManifestEntry {
            path: "private-assets/missing.txt".into(),
            id,
            type_path: "opaque".into(),
        }],
    };
    write_pak(
        &staged.root().join("base.fpak"),
        cooked.root(),
        &[(id, TypePathHash(0))],
        &manifest,
        &PakWriteOptions::default(),
    )?;
    let policy = Policy {
        deny_terms: vec!["private-assets".into()],
        ..Policy::default()
    };
    let report = audit_tree(staged.root(), &policy)?;
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.source == "name" && f.path == format!("base.fpak::{id}"))
    );
    Ok(())
}

#[test]
fn duplicate_pak_ids_are_an_incomplete_audit_not_a_clean_package() -> Result {
    use regia_asset::{AssetId, AssetManifest};
    use regia_asset_pipeline::pak_writer::{PakWriteOptions, write_pak};
    use regia_core::id::TypePathHash;
    let cooked = Tree::new();
    let staged = Tree::new();
    let id = AssetId::NIL;
    cooked.write(&regia_asset::cooked_file_name(id), b"clean payload")?;
    write_pak(
        &staged.root().join("base.fpak"),
        cooked.root(),
        &[(id, TypePathHash(0)); 2],
        &AssetManifest::default(),
        &PakWriteOptions::default(),
    )?;
    assert_eq!(
        audit_tree(staged.root(), &Policy::default())
            .expect_err("duplicate id")
            .code,
        "package_hygiene_pak"
    );
    Ok(())
}

#[test]
fn public_key_headers_and_bare_service_prefixes_are_not_secrets() -> Result {
    let tree = Tree::new();
    tree.write("public.txt", b"-----BEGIN PUBLIC KEY-----\nghp_short\nsk-short\nhttps://hooks.slack.com/services/\nhttps://discord.com/api/webhooks/\n")?;
    assert!(check_tree(tree.root(), &Policy::default()).is_ok());
    Ok(())
}

#[test]
fn build_path_in_json_scrubbed() -> Result {
    let tree = Tree::new();
    let original = concat!(
        r#"{"mesh":"/home/builder/game/assets/mesh.bin","#,
        r#""srv":"/srv/disk/out/a.gltf","#,
        r#""media":"/media/vol/proj/b.txt","#,
        r#""tmp":"/tmp/work/c.txt","#,
        r#""win":"C:\\Users\\builder\\proj\\d.txt"}"#,
    );
    tree.write("content/scene.json", original.as_bytes())?;
    tree.write("content/scene.ron", br#"("/home/builder/game/x.ron")"#)?;
    tree.write("content/scene.gltf", br#"{"uri":"/srv/disk/models/hero.gltf"}"#)?;
    tree.write("content/note.txt", b"see /media/vol/docs/readme.txt")?;
    tree.write("content/keep.toml", br#"path = "/home/builder/game/x""#)?;
    let report = prepare_stage(tree.root(), &Policy::default(), &StageRules::default())?;
    let json = std::fs::read_to_string(tree.root().join("content/scene.json"))?;
    assert!(!json.contains("/home/"), "{json}");
    assert!(!json.contains("/srv/"), "{json}");
    assert!(!json.contains("/media/"), "{json}");
    assert!(!json.contains("/tmp/"), "{json}");
    assert!(!json.contains("C:\\\\Users"), "{json}");
    assert!(json.contains("game/assets/mesh.bin"), "{json}");
    assert!(json.contains("out/a.gltf"), "{json}");
    assert!(json.contains("proj/b.txt"), "{json}");
    assert!(json.contains("work/c.txt"), "{json}");
    assert!(json.contains("proj/d.txt"), "{json}");
    let ron = std::fs::read_to_string(tree.root().join("content/scene.ron"))?;
    assert!(ron.contains("game/x.ron") && !ron.contains("/home/"), "{ron}");
    let gltf = std::fs::read_to_string(tree.root().join("content/scene.gltf"))?;
    assert!(
        gltf.contains("models/hero.gltf") && !gltf.contains("/srv/"),
        "{gltf}"
    );
    let txt = std::fs::read_to_string(tree.root().join("content/note.txt"))?;
    assert!(
        txt.contains("docs/readme.txt") && !txt.contains("/media/"),
        "{txt}"
    );
    let toml = std::fs::read(tree.root().join("content/keep.toml"))?;
    assert_eq!(toml, br#"path = "/home/builder/game/x""#);
    assert!(report.actions.iter().any(|hit| hit.rule == "build_path"));
    let dumped = serde_json::to_string(&report)?;
    assert!(!dumped.contains("/home/builder"), "{dumped}");
    tree.write("content/scene.json", original.as_bytes())?;
    let reject = Policy {
        path_scrub: PathScrub::Reject,
        ..Policy::default()
    };
    let error = prepare_stage(tree.root(), &reject, &StageRules::default()).expect_err("reject");
    assert_eq!(error.code, "package_hygiene_failed");
    let message = error.to_string();
    let rejected = error.report.expect("report");
    assert!(rejected.findings.iter().any(|hit| hit.rule == "build_path"));
    assert!(!message.contains("/home/builder"), "{message}");
    assert_eq!(
        std::fs::read_to_string(tree.root().join("content/scene.json"))?,
        original
    );
    Ok(())
}

#[test]
fn dev_flag_probe_rejects_a_dev_build() -> Result {
    let tree = Tree::new();
    tree.write("player", b"\x7fELFregia-dev-feature")?;
    let error = prepare_stage(tree.root(), &Policy::default(), &StageRules::default()).expect_err("marker");
    assert_eq!(error.code, "package_hygiene_failed");
    let report = error.report.expect("report");
    assert!(
        report
            .findings
            .iter()
            .any(|hit| hit.rule == "dev_flag" && hit.path == "player")
    );
    assert!(!serde_json::to_string(&report)?.contains("regia-dev-feature"));
    #[cfg(unix)]
    {
        let live = Tree::new();
        write_probe_script(
            &live.root().join("player"),
            "#!/bin/sh\nif [ \"$1\" = \"--regia-dev-probe\" ]; then exit 0; fi\nif [ \"$1\" = \"--regia-cheat-probe\" ]; then exit 0; fi\nexit 2\n",
        )?;
        let error =
            prepare_stage(live.root(), &Policy::default(), &StageRules::default()).expect_err("probe");
        let report = error.report.expect("report");
        assert!(
            report
                .findings
                .iter()
                .any(|hit| hit.rule == "dev_flag" && hit.source == "probe")
        );
        let clean = Tree::new();
        write_probe_script(&clean.root().join("player"), "#!/bin/sh\nexit 2\n")?;
        let report = prepare_stage(clean.root(), &Policy::default(), &StageRules::default())?;
        assert!(report.findings.iter().all(|hit| hit.rule != "dev_flag"));
    }
    Ok(())
}

#[test]
fn a_stage_file_outside_the_allowlist_fails() -> Result {
    let tree = Tree::new();
    tree.write("content/a.fpak", b"pak")?;
    tree.write("player", b"ship")?;
    tree.write("secrets.env", b"nope")?;
    let stage = StageRules {
        stage_allow: Some(vec!["content/**".into(), "player".into()]),
        ..StageRules::default()
    };
    let error = prepare_stage(tree.root(), &Policy::default(), &stage).expect_err("allowlist");
    assert_eq!(error.code, "package_hygiene_failed");
    let report = error.report.expect("report");
    assert!(
        report
            .findings
            .iter()
            .any(|hit| hit.rule == "stage_allow" && hit.path == "secrets.env")
    );
    assert!(
        report
            .findings
            .iter()
            .all(|hit| hit.path != "content/a.fpak" && hit.path != "player")
    );
    Ok(())
}

#[test]
fn internal_notes_dropped_legal_notices_kept() -> Result {
    let tree = Tree::new();
    tree.write("content/maps/README.md", b"internal map notes")?;
    tree.write("content/NOTES", b"scratch")?;
    tree.write("content/TODO.txt", b"later")?;
    tree.write("content/design.md", b"# draft")?;
    tree.write("content/legal/NOTICE.md", b"ship this notice")?;
    tree.write("README.md", b"root stays")?;
    let stage = StageRules {
        legal_notices: vec!["content/legal/**".into()],
        ..StageRules::default()
    };
    let report = prepare_stage(tree.root(), &Policy::default(), &stage)?;
    assert!(!tree.root().join("content/maps/README.md").exists());
    assert!(!tree.root().join("content/NOTES").exists());
    assert!(!tree.root().join("content/TODO.txt").exists());
    assert!(!tree.root().join("content/design.md").exists());
    assert_eq!(
        std::fs::read(tree.root().join("content/legal/NOTICE.md"))?,
        b"ship this notice"
    );
    assert_eq!(std::fs::read(tree.root().join("README.md"))?, b"root stays");
    assert!(
        report
            .actions
            .iter()
            .any(|hit| hit.rule == "internal_notes" && hit.path == "content/maps/README.md")
    );
    assert!(
        report
            .actions
            .iter()
            .all(|hit| hit.path != "content/legal/NOTICE.md")
    );
    Ok(())
}

#[test]
fn binary_files_are_never_rewritten() -> Result {
    let tree = Tree::new();
    let bin = b"\0/home/builder/game/secret.bin/srv/disk/a";
    let broken = b"{\"p\":\"/home/builder/game/a.txt\"}\0";
    let glb = b"glTF/home/builder/game/mesh.bin";
    tree.write("content/blob.bin", bin)?;
    tree.write("content/broken.json", broken)?;
    tree.write("content/mesh.glb", glb)?;
    tree.write("content/ok.json", br#"{"p":"/home/builder/game/ok.txt"}"#)?;
    let report = prepare_stage(tree.root(), &Policy::default(), &StageRules::default())?;
    assert_eq!(std::fs::read(tree.root().join("content/blob.bin"))?, bin);
    assert_eq!(std::fs::read(tree.root().join("content/broken.json"))?, broken);
    assert_eq!(std::fs::read(tree.root().join("content/mesh.glb"))?, glb);
    let ok = std::fs::read_to_string(tree.root().join("content/ok.json"))?;
    assert!(ok.contains("game/ok.txt") && !ok.contains("/home/"), "{ok}");
    assert!(report.actions.iter().any(|hit| hit.rule == "build_path"));
    let dumped = serde_json::to_string(&report)?;
    assert!(!dumped.contains("/home/builder"));
    Ok(())
}

#[test]
fn stage_rules_parse_from_the_package_table() -> Result {
    let stage = StageRules::from_manifest(
        "[package]\nstage_allow=['content/**','player']\nlegal_notices=['content/legal/**']\nextra=1\n",
    )?;
    assert_eq!(stage.stage_allow.expect("allow").len(), 2);
    assert_eq!(stage.legal_notices, ["content/legal/**"]);
    let absent = StageRules::from_manifest("[package_hygiene]\nscan_names=false\n")?;
    assert!(absent.stage_allow.is_none());
    assert_eq!(
        StageRules::from_manifest("[package]\nstage_allow=['/abs']\n")
            .expect_err("absolute glob")
            .code,
        "package_hygiene_config"
    );
    Ok(())
}

#[test]
fn audit_reports_a_build_path_without_rewriting() -> Result {
    let project = Tree::new();
    project.write("Regia.toml", b"[project]\nname='Sample'\n")?;
    let stage = project.root().join("stage");
    std::fs::create_dir_all(&stage)?;
    let original = br#"{"p":"/home/builder/game/a.txt"}"#;
    regia_test_utils::write_atomic(&stage.join("a.json"), original)?;
    let error = check_project_tree(project.root(), &stage).expect_err("build path");
    assert_eq!(error.code, "package_hygiene_failed");
    let report = error.report.expect("report");
    assert!(report.findings.iter().any(|hit| hit.rule == "build_path"));
    assert!(report.actions.is_empty());
    assert_eq!(std::fs::read(stage.join("a.json"))?, original);
    Ok(())
}

#[cfg(unix)]
fn write_probe_script(path: &std::path::Path, body: &str) -> Result {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o755)
        .open(path)?;
    file.write_all(body.as_bytes())?;
    Ok(())
}

/// Every agent guide topic ships inside any game binary that links the agent, so no topic may hold
/// a secret-shaped sample: the default shipping audit would refuse the game (the arena sample's
/// `package_e2e` red at a03439ea2: `secret_api_key` in the `package-hygiene` topic; ADR-8786).
#[test]
fn agent_guide_topics_pass_the_default_secret_scan() -> Result {
    let tree = Tree::new();
    for topic in regia_agent::GUIDE_TOPICS {
        let text = format!("{}\n{}\n", topic.summary, topic.text);
        tree.write(&format!("guide/{}.txt", topic.name), text.as_bytes())?;
    }
    let report = audit_tree(tree.root(), &Policy::default())?;
    let hits: Vec<String> = report
        .findings
        .iter()
        .map(|finding| format!("{} in {}", finding.rule, finding.path))
        .collect();
    assert!(report.complete);
    assert!(hits.is_empty(), "guide topics carry secret-shaped text: {hits:?}");
    Ok(())
}
