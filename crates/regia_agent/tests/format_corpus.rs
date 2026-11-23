// SPDX-License-Identifier: MPL-2.0
//! The frozen-format promise, checked with old bytes (ADR-2743, `docs/specs/12-assets.md` § 4a).
//!
//! `cargo xtask check-formats` hashes only the current serde shape; it cannot see a migrator that broke. This
//! test reads every fixture of `tests/fixtures/format-corpus/<format>/` (old versions are real bytes taken from
//! history, the current version is a frozen copy of the registry fixture; `index.ron` in each folder says where
//! each file came from) through the loaders of the fuzz-target registry and checks:
//!
//! 1. it loads and the result is at the format's current version;
//! 2. the re-encoding loads again and re-encodes to the same text or bytes (it "reads back equal");
//! 3. migrating is idempotent (loading the migrated output changes nothing, so a second run of `upgrade-project`
//!    changes no bytes);
//! 4. a copy whose version is patched to current + 1 is refused with an error that names the version: never a
//!    panic, never a silent load.
//!
//! Formats whose loaders the registry cannot reach (`COVERED_ELSEWHERE` in `fuzz_targets.rs`) run the same checks
//! in their own crate's `format_corpus.rs`, from the same directory.

mod format_corpus_support;

use format_corpus_support::{corpus_dir, load_all_indexes};
use regia_agent::fuzz_targets::{self, FuzzInput, FuzzTarget};

/// Run `target`'s loader on `bytes`, turning a panic into an error string prefixed `panic:`.
fn load(target: &FuzzTarget, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match target.input {
        FuzzInput::Text { load, .. } => {
            let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
            load(text).map(String::into_bytes)
        }
        FuzzInput::Bytes { load, .. } => load(bytes),
    }));
    match result {
        Ok(r) => r,
        Err(_) => Err("panic: the loader panicked".to_string()),
    }
}

/// Component fragments from the format docs (`"type::path": ( ... ),`) are map entries of a level's component
/// table; the reflected codec reads a whole map, so wrap them the way a level does.
const FRAGMENT_FORMATS: &[&str] = &["post-volume", "procgen-generator"];

/// The level the streaming fixtures belong to (the registry's constant).
fn stream_level() -> regia_scene::level::LevelId {
    regia_scene::level::LevelId(regia_core::id::Guid::from_u128(0x5747_7265_616d))
}

/// Version patching for formats whose version is not a text key or a header word.
fn patch(format: &str, bytes: &[u8], version: u32) -> Option<Vec<u8>> {
    match format {
        "world-index" => {
            let mut index = regia_world::format::WorldIndex::from_bytes(bytes, stream_level()).ok()?;
            index.format_version = version;
            index.to_bytes().ok()
        }
        "cell-blob" => {
            let coord = regia_scene::stream_grid::CellCoord::new(1, -2);
            let id = regia_world::format::cell_blob_id(stream_level(), coord);
            let hash = *blake3::hash(bytes).as_bytes();
            let mut blob = regia_world::format::CellBlob::from_bytes(bytes, id, &hash).ok()?;
            blob.format_version = version;
            blob.to_bytes(id, Vec::new()).ok()
        }
        // A postcard table inside a cooked (FCKD) envelope: the version is a field of the decoded table.
        "hlod-voxels" => {
            let mut table = regia_world::hlod_voxels::HlodVoxels::from_bytes(bytes, stream_level()).ok()?;
            table.format_version = version;
            table.to_bytes().ok()
        }
        "lobby-keys" => {
            let text = std::str::from_utf8(bytes).ok()?;
            let at = text.find("\"regia.v\": \"")? + "\"regia.v\": \"".len();
            let end = at + text[at..].find('"')?;
            Some(format!("{}{version}{}", &text[..at], &text[end..]).into_bytes())
        }
        // The jewelry contracts carry their version in the `schema` envelope (`jewelry.<name>/<major>`);
        // the loader refuses any other major (contract_unsupported_major).
        "jewelry-build" | "jewelry-material-library" | "jewelry-part-kit" | "jewelry-product-package" => {
            let text = std::str::from_utf8(bytes).ok()?;
            let key = text.find("\"schema\"")?;
            let slash = text[key..].find('/')? + key;
            let end = slash + text[slash..].find('"')?;
            Some(format!("{}{version}{}", &text[..slash + 1], &text[end..]).into_bytes())
        }
        _ => format_corpus_support::with_version(format, bytes, version),
    }
}

/// The registry target that covers `format`, if the registry reaches it.
fn target_for(format: &str) -> Option<&'static FuzzTarget> {
    fuzz_targets::targets().iter().find(|t| t.format == Some(format))
}

#[test]
fn every_corpus_fixture_migrates_round_trips_and_refuses_the_future() {
    let mut problems = Vec::new();
    let mut checked = 0usize;
    for (format, entries) in load_all_indexes() {
        let Some(target) = target_for(&format) else {
            if !format_corpus_support::COVERED_ELSEWHERE.contains(&format.as_str()) {
                problems.push(format!(
                    "{format}: no fuzz-registry loader and not in COVERED_ELSEWHERE: no test loads its fixtures"
                ));
            }
            continue; // checked in the owning crate's own test (COVERED_ELSEWHERE)
        };
        checked += entries.len();
        let wrap = FRAGMENT_FORMATS.contains(&format.as_str());
        let l = |b: &[u8]| {
            let is_fragment = String::from_utf8_lossy(b)
                .lines()
                .find(|l| !l.trim_start().starts_with("//") && !l.trim().is_empty())
                .is_some_and(|l| l.trim_start().starts_with('"'));
            if wrap && is_fragment {
                let text = std::str::from_utf8(b).map_err(|e| e.to_string())?;
                load(target, format!("{{\n{text}\n}}").as_bytes())
            } else {
                load(target, b)
            }
        };
        problems.extend(if target.round_trip {
            format_corpus_support::check_format_with(
                &format,
                target.format_version,
                &l,
                &|b| format_corpus_support::version_of(&format, b),
                &|b, v| patch(&format, b, v),
            )
        } else {
            // The loader parses and migrates but does not write the result back: only "loads" and "refuses the
            // future" can be checked here. `format_corpus_upgrade` (regia_cli) checks that upgrade-project reaches
            // the current version for these.
            format_corpus_support::check_load_only(&format, target.format_version, &l)
        });
    }
    assert!(checked > 0, "the corpus is empty: {}", corpus_dir().display());
    assert!(
        problems.is_empty(),
        "{} problems:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

#[test]
fn the_registry_formats_each_have_a_current_fixture_or_a_reason() {
    let indexes = load_all_indexes();
    let mut missing = Vec::new();
    for t in fuzz_targets::targets() {
        let Some(format) = t.format else { continue };
        let ok = indexes
            .get(format)
            .is_some_and(|es| es.iter().any(|e| e.version == t.format_version));
        if !ok
            && !format_corpus_support::NO_CURRENT_FIXTURE
                .iter()
                .any(|(f, _)| *f == format)
        {
            missing.push(format!("{format} (v{})", t.format_version));
        }
    }
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "formats without a frozen fixture of their current version (bless them with the serialized sequence in \
         book/changing-a-content-format.md: REGIA_BLESS_FORMAT_CORPUS=1 on every crate's `bless` tests, \
         --test-threads=1, then `bless_the_frozen_table`): {missing:?}"
    );
}

/// Current-version files checked in elsewhere, for formats whose registry fixture is older and whose loader does
/// not re-encode (so the loader cannot migrate it for us).
const CURRENT_SOURCES: &[(&str, &str)] = &[("ui", "crates/regia_ui/tests/fixtures/ui.v11.example.ui.ron")];

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Writes the frozen fixture of every registry format's current version that the corpus lacks. Run once per
/// format-version bump, with `REGIA_BLESS_FORMAT_CORPUS=1`: a registry fixture already at the current version is
/// copied as it is, an older one (the registry keeps some examples at the version they were written) is
/// migrated by the loader, so the file is the current writer's output. Old versions are never made this way.
#[test]
fn bless_missing_current_fixtures() {
    if !format_corpus_support::blessing() {
        return;
    }
    let mut done = std::collections::BTreeSet::new();
    for t in fuzz_targets::targets() {
        let Some(format) = t.format else { continue };
        if !done.insert(format) {
            continue;
        }
        let raw = match t.input {
            FuzzInput::Text { fixture, .. } => fixture().into_bytes(),
            FuzzInput::Bytes { fixture, .. } => fixture(),
        };
        let at = format_corpus_support::version_of(format, &raw);
        // A fixture already stamped at the current version, or one with no readable version when the
        // format has no version field at all (NO_VERSION_FIELD), is copied as it is. A fixture with
        // no readable version whose current layout HAS one (cue v1, say) falls through to the
        // loader below, so the frozen file is really the current writer's output — never old bytes
        // relabeled.
        let (bytes, origin) = if at == Some(t.format_version)
            || (at.is_none()
                && format_corpus_support::NO_VERSION_FIELD
                    .iter()
                    .any(|(f, _)| *f == format))
        {
            (raw, "frozen copy of the fuzz-target registry fixture".to_string())
        } else if let Some(path) = CURRENT_SOURCES.iter().find(|(f, _)| *f == format).map(|(_, p)| p) {
            let bytes = std::fs::read(repo_root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"));
            (bytes, format!("frozen copy of {path}"))
        } else if t.round_trip {
            match load(t, &raw) {
                Ok(migrated) => (
                    migrated,
                    format!(
                        "registry fixture (version {}) loaded and re-encoded by the loader at version {}",
                        at.map_or("unknown".into(), |v| v.to_string()),
                        t.format_version
                    ),
                ),
                Err(e) => {
                    eprintln!("{format}: registry fixture does not load: {e}");
                    continue;
                }
            }
        } else {
            eprintln!(
                "{format}: the registry fixture is old and its loader does not re-encode: add a CURRENT_SOURCES entry"
            );
            continue;
        };
        let ext = match t.input {
            FuzzInput::Bytes { .. } => "bin",
            FuzzInput::Text { .. } if serde_json::from_slice::<serde_json::Value>(&bytes).is_ok() => "json",
            FuzzInput::Text { .. } => "ron",
        };
        format_corpus_support::bless_current(format, t.format_version, ext, &bytes, &origin);
    }
}

/// FNV-1a, 64 bit.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Every fixture file under the corpus (not the index files): `<format>/<file>` → (length, FNV-1a).
fn corpus_files() -> std::collections::BTreeMap<String, (usize, u64)> {
    let mut all = std::collections::BTreeMap::new();
    for format in std::fs::read_dir(corpus_dir())
        .expect("read the corpus")
        .flatten()
    {
        if !format.path().is_dir() {
            continue;
        }
        for file in std::fs::read_dir(format.path())
            .expect("read a format folder")
            .flatten()
        {
            let name = file.file_name().to_string_lossy().into_owned();
            if name == "index.ron" {
                continue;
            }
            let bytes = std::fs::read(file.path()).expect("read a fixture");
            all.insert(
                format!("{}/{name}", format.file_name().to_string_lossy()),
                (bytes.len(), fnv1a(&bytes)),
            );
        }
    }
    all
}

fn frozen_table() -> std::collections::BTreeMap<String, (usize, u64)> {
    let text = std::fs::read_to_string(corpus_dir().join("FROZEN.txt")).unwrap_or_default();
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let mut parts = l.split_whitespace();
            let (Some(path), Some(len), Some(hash)) = (parts.next(), parts.next(), parts.next()) else {
                panic!("FROZEN.txt: bad line {l:?}");
            };
            (
                path.to_string(),
                (
                    len.parse().expect("a length"),
                    u64::from_str_radix(hash, 16).expect("a hex hash"),
                ),
            )
        })
        .collect()
}

/// The fixtures are frozen: a file that changed (a "re-blessed" old fixture) or one nobody listed fails. Add new
/// files with the bless sequence in `book/changing-a-content-format.md` (`REGIA_BLESS_FORMAT_CORPUS=1`, every
/// crate's `bless` tests with `--test-threads=1`, then `bless_the_frozen_table` last).
#[test]
fn the_corpus_files_are_frozen() {
    let have = corpus_files();
    let frozen = frozen_table();
    let mut problems = Vec::new();
    for (path, (len, hash)) in &have {
        match frozen.get(path) {
            None => problems.push(format!("{path}: not in FROZEN.txt (bless it)")),
            Some((l, h)) if (l, h) != (len, hash) => {
                problems.push(format!(
                    "{path}: changed (frozen {l} bytes {h:016x}, now {len} bytes {hash:016x})"
                ));
            }
            Some(_) => {}
        }
    }
    for path in frozen.keys().filter(|p| !have.contains_key(*p)) {
        problems.push(format!("{path}: listed in FROZEN.txt but gone"));
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Rewrites `FROZEN.txt` from the fixtures on disk. Run last in the bless sequence
/// (`book/changing-a-content-format.md`): entries whose file is gone are dropped, new files are added, a
/// file that changed keeps its line and fails `the_corpus_files_are_frozen` (a changed fixture is a failing
/// test, never silently re-frozen).
#[test]
fn bless_the_frozen_table() {
    if !format_corpus_support::blessing() {
        return;
    }
    let frozen = corpus_files();
    let mut text = String::from(
        "# Frozen corpus files: path, length, FNV-1a 64 (hex). Never edit a line: a changed fixture is a failing test.\n",
    );
    for (path, (len, hash)) in frozen {
        text.push_str(&format!("{path} {len} {hash:016x}\n"));
    }
    std::fs::write(corpus_dir().join("FROZEN.txt"), text).expect("write FROZEN.txt");
}
