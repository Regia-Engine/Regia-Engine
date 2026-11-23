// SPDX-License-Identifier: MPL-2.0
//! Aggregate audit memory, matcher and work budgets (ADR-8244).
//!
//! These checks fail on a scanner that keeps a whole file or pak in memory, splits an
//! unbounded pattern across chunks, or compiles every rule before applying a matcher cap.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use regia_agent::cook_package::RegiaCliProgram;
use regia_agent::{AgentConfig, AgentPermissions, LocalReply, call_local};
use regia_app::prelude::*;
use regia_cli::package_hygiene::{self, Policy, audit_tree};
use serde_json::{Value, json};

struct Tree(PathBuf);

impl Tree {
    fn new(label: &str) -> Self {
        Self(regia_test_utils::unique_temp_dir(label))
    }

    fn write(&self, path: &str, data: &[u8]) -> anyhow::Result<()> {
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

struct HookGuard;

impl HookGuard {
    fn arm() -> Self {
        Self
    }
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        package_hygiene::set_mutation_probe(None);
        package_hygiene::set_pak_owned_reads(false);
    }
}

fn stream_policy(window: u64, memory: u64) -> Policy {
    Policy {
        scan_names: false,
        scan_secrets: false,
        scan_emails: false,
        scan_window_bytes: window,
        max_memory_bytes: memory,
        ..Policy::default()
    }
}

fn content_offset(report: &package_hygiene::Report, rule: &str) -> Option<usize> {
    report
        .findings
        .iter()
        .find(|hit| hit.rule == rule && hit.source == "content")
        .map(|hit| hit.offset)
}

fn literal_at(len: usize, at: usize, needle: &[u8]) -> Vec<u8> {
    let mut bytes = vec![b'x'; len];
    bytes[at..at + needle.len()].copy_from_slice(needle);
    bytes
}

#[test]
fn a_literal_crossing_every_window_boundary_matches_a_contiguous_scan() -> anyhow::Result<()> {
    let needle = b"ABCD";
    let len = 64;
    let window = 16u64;
    let memory = 48u64;
    assert!(len as u64 > memory, "the file must not fit in the memory budget");
    let streaming = Policy {
        forbidden_strings: vec!["ABCD".into()],
        ..stream_policy(window, memory)
    };
    let contiguous = Policy {
        forbidden_strings: vec!["ABCD".into()],
        ..stream_policy(len as u64, len as u64)
    };
    for start in 0..=(len - needle.len()) {
        let bytes = literal_at(len, start, needle);
        let want = bytes.windows(needle.len()).position(|window| window == needle);
        let tree = Tree::new("hygiene-boundary");
        tree.write("payload.bin", &bytes)?;
        let streamed = audit_tree(tree.root(), &streaming)?;
        let whole = audit_tree(tree.root(), &contiguous)?;
        assert_eq!(content_offset(&streamed, "forbidden_string_1"), want);
        assert_eq!(content_offset(&whole, "forbidden_string_1"), want);
        assert_eq!(content_offset(&streamed, "forbidden_string_1"), Some(start));
        assert!(streamed.peak_memory_bytes <= memory);
        assert!(streamed.peak_memory_bytes < len as u64);
        assert_eq!(streamed.work, len as u64);
        assert!(streamed.complete);
        assert!(streamed.incomplete_reason.is_empty());
    }
    Ok(())
}

#[test]
fn an_ascii_word_keeps_its_offset_across_window_edges() -> anyhow::Result<()> {
    let needle = b"QQQQ";
    let len = 64;
    let policy = Policy {
        deny_terms: vec!["word:QQQQ".into()],
        ..stream_policy(16, 48)
    };
    for start in 0..=(len - needle.len() - 2) {
        let mut bytes = vec![b'x'; len];
        bytes[start] = b' ';
        bytes[start + 1..start + 1 + needle.len()].copy_from_slice(needle);
        bytes[start + 1 + needle.len()] = b' ';
        let tree = Tree::new("hygiene-word");
        tree.write("payload.bin", &bytes)?;
        let report = audit_tree(tree.root(), &policy)?;
        assert_eq!(content_offset(&report, "deny_term_1"), Some(start + 1));
        assert!(report.peak_memory_bytes <= 48);
        assert!(report.peak_memory_bytes < len as u64);
    }
    for start in 0..=(len - needle.len() - 1) {
        let mut bytes = vec![b'x'; len];
        bytes[start..start + needle.len()].copy_from_slice(needle);
        let tree = Tree::new("hygiene-word-miss");
        tree.write("payload.bin", &bytes)?;
        let report = audit_tree(tree.root(), &policy)?;
        assert_eq!(content_offset(&report, "deny_term_1"), None);
    }
    Ok(())
}

#[test]
fn an_unbounded_pattern_is_found_when_the_file_fits_and_refused_when_it_does_not() -> anyhow::Result<()> {
    let pattern = "START.*END";
    let regex = regex::bytes::Regex::new(pattern)?;
    let small = b"xxSTARTyyyyENDzz";
    let fit = Policy {
        forbidden_patterns: vec![pattern.into()],
        ..stream_policy(8, 64)
    };
    let tree = Tree::new("hygiene-unbounded-fit");
    tree.write("note.txt", small)?;
    let report = audit_tree(tree.root(), &fit)?;
    let want = regex.find(small).map(|hit| hit.start());
    assert_eq!(content_offset(&report, "forbidden_pattern_1"), want);
    assert!(report.complete);

    let mut large = vec![b'q'; 200];
    large[..5].copy_from_slice(b"START");
    large[180..183].copy_from_slice(b"END");
    let want_large = regex.find(&large).map(|hit| hit.start());
    assert!(
        want_large.is_some(),
        "the fixture must contain a real cross-window match"
    );
    let refuse = Policy {
        forbidden_patterns: vec![pattern.into()],
        ..stream_policy(16, 64)
    };
    let wide = Tree::new("hygiene-unbounded-refuse");
    wide.write("note.txt", &large)?;
    let error = audit_tree(wide.root(), &refuse).expect_err("unbounded input over the memory budget");
    assert_eq!(error.code, "package_hygiene_limit");
    let partial = error.report.expect("partial report");
    assert!(!partial.complete);
    assert_eq!(partial.incomplete_reason, "unbounded_regex");
    assert!(partial.peak_memory_bytes <= 64);
    assert!(partial.peak_memory_bytes < large.len() as u64);
    assert!(
        partial.findings.is_empty(),
        "a refused scan must not report a partial regex hit"
    );

    let accepted = Policy {
        forbidden_patterns: vec![pattern.into()],
        ..stream_policy(large.len() as u64, large.len() as u64)
    };
    let again = Tree::new("hygiene-unbounded-whole");
    again.write("note.txt", &large)?;
    let report = audit_tree(again.root(), &accepted)?;
    assert_eq!(content_offset(&report, "forbidden_pattern_1"), want_large);
    Ok(())
}

#[test]
fn the_matcher_budget_rejects_before_compiling_the_rule_that_does_not_fit() {
    let policy = Policy {
        forbidden_strings: vec!["0123456789".into(), "abcdefghij".into()],
        scan_secrets: false,
        scan_names: false,
        max_matcher_bytes: 1000,
        ..Policy::default()
    };
    let error = audit_tree(Path::new("matcher-budget-does-not-read"), &policy)
        .expect_err("second literal exceeds the matcher budget");
    assert_eq!(error.code, "package_hygiene_config");
    assert_eq!(error.location, "matcher_budget");
    assert!(error.report.is_none());
    assert_eq!(error.matchers_compiled, 1);
    assert_eq!(error.matcher_bytes, 10 * 64 + 256);
    assert!(error.matcher_bytes <= policy.max_matcher_bytes);
}

#[test]
fn peak_charged_bytes_stay_flat_as_files_grow() -> anyhow::Result<()> {
    let window = 4096u64;
    let policy = Policy {
        forbidden_strings: vec!["ENDMARK".into()],
        ..stream_policy(window, window)
    };
    let mut peaks = Vec::new();
    for len in [8192usize, 16384, 32768] {
        let bytes = literal_at(len, len - b"ENDMARK".len(), b"ENDMARK");
        let tree = Tree::new("hygiene-scale");
        tree.write("payload.bin", &bytes)?;
        let report = audit_tree(tree.root(), &policy)?;
        assert_eq!(
            content_offset(&report, "forbidden_string_1"),
            Some(len - b"ENDMARK".len())
        );
        assert_eq!(report.work, len as u64);
        assert!(report.peak_memory_bytes <= window);
        assert!(report.peak_memory_bytes < len as u64);
        assert_eq!(report.peak_memory_bytes, window);
        peaks.push(report.peak_memory_bytes);
    }
    assert!(peaks.windows(2).all(|pair| pair[0] == pair[1]));
    Ok(())
}

#[test]
fn a_work_budget_and_an_overwide_pattern_fail_closed() -> anyhow::Result<()> {
    let tree = Tree::new("hygiene-work");
    tree.write("payload.bin", &[b'x'; 64])?;
    let work = Policy {
        max_work: 16,
        ..stream_policy(16, 32)
    };
    let error = audit_tree(tree.root(), &work).expect_err("work budget");
    assert_eq!(error.code, "package_hygiene_limit");
    let report = error.report.expect("partial");
    assert!(!report.complete);
    assert_eq!(report.incomplete_reason, "work_budget");
    assert!(report.peak_memory_bytes <= 32);
    assert!(report.work <= 16);

    let wide = Policy {
        forbidden_patterns: vec!["A{80}".into()],
        ..stream_policy(16, 32)
    };
    let error = audit_tree(tree.root(), &wide).expect_err("pattern wider than memory");
    let report = error.report.expect("partial");
    assert_eq!(error.code, "package_hygiene_limit");
    assert!(!report.complete);
    assert_eq!(report.incomplete_reason, "pattern_wider_than_memory");
    assert!(report.peak_memory_bytes <= 32);
    Ok(())
}

fn varied(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index % 251) as u8).collect()
}

fn pack_entry(dir: &Path, payload: &[u8]) -> anyhow::Result<(u64, u64)> {
    use regia_asset::{AssetId, AssetManifest};
    use regia_asset_pipeline::pak_writer::{PakWriteOptions, write_pak};
    use regia_core::id::TypePathHash;

    let cooked = Tree::new("hygiene-cooked");
    let id = AssetId::NIL;
    cooked.write(&regia_asset::cooked_file_name(id), payload)?;
    let out = dir.join("pack.fpak");
    write_pak(
        &out,
        cooked.root(),
        &[(id, TypePathHash(0))],
        &AssetManifest::default(),
        &PakWriteOptions {
            compress: false,
            ..PakWriteOptions::default()
        },
    )?;
    let file_len = std::fs::symlink_metadata(&out)?.len();
    let reader = regia_asset::pak::PakReader::open(&out)?;
    let entry = reader.entry(id).expect("written entry").size;
    Ok((file_len, entry))
}

#[test]
fn a_decompression_bomb_and_a_nested_pak_fail_closed() -> anyhow::Result<()> {
    let bomb = Tree::new("hygiene-bomb");
    let payload = varied(9000);
    let (_file_len, entry_size) = pack_entry(bomb.root(), &payload)?;
    assert!(
        entry_size > 8192,
        "entry {entry_size} must exceed the memory budget"
    );
    let policy = stream_policy(1024, 8192);
    let error = audit_tree(bomb.root(), &policy).expect_err("declared entry over the memory budget");
    assert_eq!(error.code, "package_hygiene_limit");
    let report = error.report.expect("partial");
    assert!(!report.complete);
    assert_eq!(report.incomplete_reason, "decompression_bomb");
    assert!(report.peak_memory_bytes <= 8192);
    assert!(report.peak_memory_bytes < entry_size);

    let nested = Tree::new("hygiene-nested");
    let payload = varied(2000);
    let (file_len, entry_size) = pack_entry(nested.root(), &payload)?;
    assert!(
        file_len > entry_size,
        "an owned pak must be larger than its stored entry"
    );
    let policy = stream_policy(256, file_len);
    let _guard = HookGuard::arm();
    package_hygiene::set_pak_owned_reads(true);
    let error = audit_tree(nested.root(), &policy).expect_err("pak plus entry exceed the budget");
    assert_eq!(error.code, "package_hygiene_limit");
    let report = error.report.expect("partial");
    assert!(!report.complete);
    assert_eq!(report.incomplete_reason, "nested_limit");
    assert_eq!(report.peak_memory_bytes, file_len);
    assert!(report.peak_memory_bytes <= policy.max_memory_bytes);
    assert!(report.peak_memory_bytes.saturating_add(entry_size) > policy.max_memory_bytes);
    Ok(())
}

fn append_probe(path: &Path) {
    PROBE_HITS.fetch_add(1, Ordering::SeqCst);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("probe opens the snapped file");
    file.write_all(&[0xAB]).expect("probe appends one byte");
}

static PROBE_HITS: AtomicUsize = AtomicUsize::new(0);

#[test]
fn a_changed_file_invalidates_the_scan() -> anyhow::Result<()> {
    let tree = Tree::new("hygiene-mutate");
    tree.write("payload.bin", b"stable-bytes")?;
    let before = std::fs::symlink_metadata(tree.root().join("payload.bin"))?.len();
    let _guard = HookGuard::arm();
    let hits = PROBE_HITS.load(Ordering::SeqCst);
    package_hygiene::set_mutation_probe(Some(append_probe));
    let policy = Policy {
        forbidden_strings: vec!["stable-bytes".into()],
        ..stream_policy(8, 64)
    };
    let error = audit_tree(tree.root(), &policy).expect_err("the probe changes the file");
    assert_eq!(error.code, "package_hygiene_io");
    let report = error.report.expect("partial");
    assert!(!report.complete);
    assert_eq!(report.incomplete_reason, "changed_file");
    assert!(PROBE_HITS.load(Ordering::SeqCst) > hits);
    let after = std::fs::symlink_metadata(tree.root().join("payload.bin"))?.len();
    assert!(after > before);
    assert!(content_offset(&report, "forbidden_string_1").is_none());
    Ok(())
}

fn xorshift(state: &mut u64) -> u8 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    (x & 0xff) as u8
}

#[test]
fn random_file_and_pak_framing_stays_inside_the_budget() -> anyhow::Result<()> {
    let tree = Tree::new("hygiene-fuzz");
    let mut state = 0x1234_5678_9abc_def0u64;
    let codes = [
        "package_hygiene_config",
        "package_hygiene_io",
        "package_hygiene_path",
        "package_hygiene_limit",
        "package_hygiene_pak",
        "package_hygiene_failed",
    ];
    for index in 0usize..48 {
        let len = (xorshift(&mut state) as usize) % 81;
        let mut bytes = vec![0u8; len];
        for byte in &mut bytes {
            *byte = xorshift(&mut state);
        }
        let name = if index.is_multiple_of(2) {
            format!("frame-{index}.fpak")
        } else {
            format!("frame-{index}.bin")
        };
        tree.write(&name, &bytes)?;
        let unbounded = index.is_multiple_of(3);
        let policy = Policy {
            forbidden_strings: vec!["ZZ".into()],
            forbidden_patterns: if unbounded { vec!["Q+".into()] } else { Vec::new() },
            ..stream_policy(16, 32)
        };
        match audit_tree(tree.root(), &policy) {
            Ok(report) => {
                assert!(report.complete);
                assert!(report.peak_memory_bytes <= policy.max_memory_bytes);
                assert!(report.matcher_bytes <= policy.max_matcher_bytes);
            }
            Err(error) => {
                assert!(codes.contains(&error.code), "{}", error.code);
                if let Some(report) = error.report {
                    assert!(!report.complete);
                    assert!(!report.incomplete_reason.is_empty());
                    assert!(report.peak_memory_bytes <= policy.max_memory_bytes);
                    assert!(report.matcher_bytes <= policy.max_matcher_bytes);
                }
            }
        }
        std::fs::remove_file(tree.root().join(&name))?;
    }
    Ok(())
}

fn project(label: &str, manifest: &str) -> anyhow::Result<Tree> {
    let tree = Tree::new(label);
    std::fs::create_dir_all(tree.root().join("dist/player"))?;
    regia_test_utils::write_atomic(&tree.root().join("Regia.toml"), manifest.as_bytes())?;
    Ok(tree)
}

#[test]
fn cli_json_reports_an_incomplete_secret_scan_without_dropping_the_hygiene_object() -> anyhow::Result<()> {
    let tree = project(
        "hygiene-cli",
        "[package_hygiene]\nmax_memory_bytes = 32\nscan_window_bytes = 16\n",
    )?;
    tree.write("dist/player/note.txt", &[b'a'; 100])?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["package", "--platform", "linux", "--audit-dir"])
        .arg(tree.root().join("dist/player"))
        .arg("--project")
        .arg(tree.root())
        .arg("--json")
        .output()?;
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "package_hygiene_limit");
    assert!(response["error"]["matcher_bytes"].as_u64().is_some());
    assert!(response["error"]["matchers_compiled"].as_u64().is_some());
    assert_eq!(response["hygiene"]["complete"], false);
    assert_eq!(response["hygiene"]["incomplete_reason"], "unbounded_regex");
    assert!(
        response["hygiene"]["limits"]["max_memory_bytes"]
            .as_u64()
            .expect("limit")
            <= 32
    );
    assert!(response["hygiene"]["peak_memory_bytes"].as_u64().expect("peak") <= 32);
    Ok(())
}

fn agent_world(root: &Path) -> World {
    let mut world = World::new();
    let mut config = AgentConfig::default();
    config.permissions.insert("build".into());
    world.insert_resource(AgentPermissions { config, source: None });
    world.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.to_path_buf()),
    });
    world.insert_resource(RegiaCliProgram(PathBuf::from(env!("CARGO_BIN_EXE_regia"))));
    world
}

#[test]
fn agent_package_job_reports_complete_limits() -> anyhow::Result<()> {
    let tree = project("hygiene-agent", "[project]\nname='Sample'\n")?;
    tree.write("dist/player/note.txt", b"hello\n")?;
    let mut world = agent_world(tree.root());
    let LocalReply::Done(reply) = call_local(
        &mut world,
        "agent.package",
        json!({"audit_dir": "dist/player", "platform": "linux", "confirm": true}),
        None,
    ) else {
        panic!("package start answers immediately");
    };
    let reply = reply.map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let job = reply["job"].as_str().expect("job id").to_owned();
    assert!(job.starts_with("package:"), "{job}");
    let mut status = Value::Null;
    for _ in 0..8_000_000 {
        let LocalReply::Done(polled) = call_local(&mut world, "agent.job_status", json!({"job": job}), None)
        else {
            panic!("job status answers immediately");
        };
        status = polled.map_err(|error| anyhow::anyhow!("{error:?}"))?;
        if status["finished"] == true {
            break;
        }
        thread::yield_now();
    }
    assert_eq!(status["finished"], true, "{status}");
    let hygiene = &status["result"]["report"]["hygiene"];
    assert_eq!(hygiene["complete"], true, "{status}");
    assert_eq!(hygiene["incomplete_reason"], "");
    assert_eq!(hygiene["limits"]["max_memory_bytes"], json!(32u64 << 20));
    assert_eq!(hygiene["limits"]["scan_window_bytes"], json!(1u64 << 20));
    assert_eq!(hygiene["limits"]["max_matcher_bytes"], json!(8u64 << 20));
    assert_eq!(hygiene["limits"]["max_work"], json!(1u64 << 40));
    assert_eq!(status["result"]["ok"], true, "{status}");
    Ok(())
}
