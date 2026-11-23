// SPDX-License-Identifier: MPL-2.0
//! Shared by the `format_corpus.rs` tests of every crate that owns a frozen format (ADR-2743): the index files of
//! `crates/regia_agent/tests/fixtures/format-corpus/`, reading and patching a version, and the bless helper. Other
//! crates include this file with `#[path = "../../regia_agent/tests/format_corpus_support/mod.rs"]`.
#![allow(dead_code)] // each including test uses a part

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One corpus fixture, as `index.ron` lists it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// The version the file really is at.
    pub version: u32,
    /// File name inside the format's folder.
    pub file: String,
    /// Where the bytes came from: `git <rev>:<path>`, `handwritten from <rev>` or `frozen from the registry fixture`.
    pub origin: String,
    /// What the fixture exercises.
    pub exercises: String,
}

/// The `index.ron` of one format folder.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Index {
    /// The registry name in `docs/formats/format-hashes.ron`.
    pub format: String,
    /// One entry per fixture.
    pub entries: Vec<Entry>,
}

/// `crates/regia_agent/tests/fixtures/format-corpus`, from any crate under `crates/`.
pub fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../regia_agent/tests/fixtures/format-corpus")
}

/// Fixtures whose bytes carry no readable version although other versions of the format have one, or whose
/// version field the shared reader cannot see: `(format, version)` the index may claim without a readable
/// counterpart in the file. Everything else must read back at the version its `index.ron` entry claims.
pub const UNVERSIONED_FIXTURE: &[(&str, u32)] = &[("cue", 1), ("input-overrides", 1), ("lobby-keys", 1)];

/// Formats the fuzz-target registry cannot reach whose corpus fixtures another crate's `format_corpus.rs`
/// loads (`COVERED_ELSEWHERE`). The agent test fails a corpus format that is neither here nor in the registry,
/// so a folder nobody loads cannot pass silently.
pub const COVERED_ELSEWHERE: &[&str] = &[
    "assetpkg",
    "bake",
    "cooked-clip",
    "cooked-mesh",
    // The editor's own files (ADR-7902): crates/regia_editor/tests/format_corpus.rs.
    "editor-keymap",
    "editor-layout",
    "editor-panel",
    "editor-prefs",
    "editor-profile",
    "editor-theme",
    "editor-view-state",
    "editor-window-state",
    "engine-release",
    "fracture",
    "generator",
    "import-rules",
    "inspector-pins",
    "luamod",
    "luastate",
    "pak",
    "provenance-policy",
    "save",
    "saved-searches",
    "session",
    "validation",
    "viewport-snap",
    "xr-actions",
    "xr-config",
    "xrsim",
    "launch-profiles",
    "node-tracks",
    "key-track",
    "source-lint",
    "release-chunks",
    // ADR-8890: crates/regia_net/tests/format_corpus.rs.
    "netcapture",
    // ADR-10356: the reader is a pipeline crate the runtime registry must not link;
    // crates/regia_release_chunks/tests/format_corpus.rs.
    "release-signature",
];

/// Loaders that only parse (they accept the older layout and keep its stored version number) and whose
/// "reaches the current version" is asserted by `regia_cli/tests/format_corpus_upgrade.rs` through
/// `regia upgrade-project` instead. A `round_trip` loader not listed here must re-encode at the current
/// version; the shared check fails it otherwise.
pub const PARSE_ONLY_LOADERS: &[(&str, &str)] = &[(
    "netprofile",
    "ADR-8572 (5): `NetProfile::from_ron` keeps the loaded version (`migrate_ron` and `regia upgrade-project` \
     rely on it); `format_corpus_upgrade` asserts the v1 fixture reaches v2 through `regia upgrade-project`",
)];

/// Every format folder's index, by format name. Also asserts that every fixture whose version can be read
/// is at the version its entry claims (the exceptions in [`UNVERSIONED_FIXTURE`]), so a fixture can never
/// sit in the corpus mislabeled.
pub fn load_all_indexes() -> BTreeMap<String, Vec<Entry>> {
    let mut all = BTreeMap::new();
    let dir = corpus_dir();
    let read = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in read.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let path = entry.path().join("index.ron");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let index: Index = ron::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let folder = entry.file_name().to_string_lossy().into_owned();
        assert_eq!(
            index.format,
            folder,
            "{}: `format` must be the folder name",
            path.display()
        );
        let mut seen = std::collections::BTreeSet::new();
        let mut fold_files = std::collections::BTreeSet::new();
        let mut fold_versions = std::collections::BTreeSet::new();
        for e in &index.entries {
            // A fold (ADR-7724 (b)) adds a second fixture of an unreleased version, named
            // `v<N>-<what>.<ext>`, next to the frozen `v<N>` one; any other repeat is a mistake.
            let fold = e
                .file
                .strip_prefix(&format!("v{}-", e.version))
                .is_some_and(|rest| !rest.is_empty());
            if fold {
                // Each fold fixture once; its version keeps its base `v<N>` fixture (checked after
                // the loop). Review sprint-w12-rev-g13 #2: a fold name never stood in for the base.
                assert!(
                    fold_files.insert(e.file.clone()),
                    "{}: {} listed twice",
                    path.display(),
                    e.file
                );
                fold_versions.insert(e.version);
            } else {
                assert!(
                    seen.insert(e.version),
                    "{}: version {} listed twice",
                    path.display(),
                    e.version
                );
            }
            let file = entry.path().join(&e.file);
            assert!(
                file.is_file(),
                "{}: lists {} but the file is missing",
                path.display(),
                e.file
            );
            if let Some(found) = version_of(&index.format, &std::fs::read(&file).expect("read a fixture"))
                && found != e.version
                && !UNVERSIONED_FIXTURE
                    .iter()
                    .any(|(f, v)| *f == index.format && *v == e.version)
            {
                panic!(
                    "{}: {} claims version {} but its bytes read back at version {}",
                    path.display(),
                    e.file,
                    e.version,
                    found
                );
            }
        }
        for version in &fold_versions {
            assert!(
                seen.contains(version),
                "{}: version {version} has a fold fixture but no base v{version} fixture",
                path.display()
            );
        }
        all.insert(index.format, index.entries);
    }
    all
}

/// Formats the registry reaches that have no frozen current-version fixture, with the reason.
pub const NO_CURRENT_FIXTURE: &[(&str, &str)] = &[];

/// Formats whose files carry no version field at all (v1 is implicit), with the reason: the future-version check
/// does not apply.
pub const NO_VERSION_FIELD: &[(&str, &str)] = &[
    (
        "bt",
        "a behavior tree file has no version field; its shape is the only thing that identifies v1",
    ),
    (
        "ies-profile",
        "an .ies profile is the IESNA LM-63 standard text; it has no Regia version field (the format version is the engine's cook reader, not anything in the file)",
    ),
];

/// Formats whose refusal of a future version can legitimately name something else first.
pub const FUTURE_OK_ERRORS: &[(&str, &str)] = &[(
    "motiondb-cooked",
    "content hash mismatch", // the content hash covers the version word, so a patched copy fails the hash first
)];

/// Binary formats: where the version lives. `(format, byte offset, width in bytes, little endian)`.
pub const BINARY_VERSION: &[(&str, usize, usize)] = &[
    // A 4-byte magic and a little-endian u32.
    ("lightmap", 4, 4),
    ("save", 4, 4),
    ("luastate", 4, 4),
    ("cooked-cloth", 4, 4),
    ("cooked-soft", 4, 4),
    ("fluidcache", 4, 4),
    ("frec", 4, 4),
    ("hull-set", 4, 4),
    ("nav-cell", 4, 4),
    ("scatter-paint", 4, 4),
    ("pgout", 4, 4),
    ("pak", 4, 4),
    // An 8-byte magic and a little-endian u32.
    ("asset-registry", 8, 4),
    ("chunk-map", 8, 4),
    ("chunk-why", 8, 4),
    ("cooked-video", 8, 4),
    // A postcard payload that starts with its u32 version: little endian, or one varint byte for small values.
    ("chooser-cooked", 0, 4),
    ("curves-cooked", 0, 4),
    ("data-table-cooked", 0, 4),
    ("motiondb-cooked", 0, 4),
    ("abilities-cooked", 0, 4),
    ("convc", 0, 1),
    ("collision-set", 0, 1),
    ("terrain-weights", 0, 1),
    ("grass-density", 0, 1),
    // The terrain edit-layers sidecar: a postcard varint version first (ADR-7825).
    ("terrain-editlayers", 0, 1),
    // The deterministic navmesh: `NVM2` and a little-endian u32 version (ADR-8174).
    ("nav-det-navmesh", 4, 4),
    // The cooked terrain index (postcard varint version first) and its exact heights (magic
    // `RTFH`, u32 LE version), ADR-8024.
    ("terrain-index", 0, 1),
    ("terrain-final-heights", 4, 4),
    ("server-players", 0, 1),
    // An 8-byte magic and a little-endian u32 (same as the 4-byte group, wider magic).
    ("bake-checkpoint", 8, 4),
    // 4-byte magics registered after the corpus branch base.
    ("fcap", 4, 4),
    ("viewer-env", 4, 4),
    ("voxel-far", 4, 4),
    ("cooked-sound", 4, 4),
    ("gpu-mesh", 4, 4),
    // integ-v049a: the `.netdemo` header (`RNETDEMO` and a little-endian u32) and the cooked prefab
    // instance set (postcard, `format_version` first: one varint byte at v1).
    ("netdemo", 8, 4),
    ("prefab-instances", 0, 1),
    // ADR-9070: the `.regiatrace` header (`RGTR` and a little-endian u32).
    ("regiatrace", 4, 4),
    // ADR-8112: the cooked `<map>.meshlayers` (`RGML`, then the header's postcard varint version: one byte at v1).
    ("terrain-mesh-layers-cooked", 4, 1),
    // ADR-8890: the `.netcapture` header (`RNETCAPT` and a little-endian u32).
    ("netcapture", 8, 4),
    // ADR-8513: the `.lsreplay` header (`RGLSRPLY` and a little-endian u32).
    ("lsreplay", 8, 4),
];

/// JSON formats whose version key is named after the format rather than `version` / `format_version`
/// (ADR-8639): `(format, key)`.
pub const JSON_VERSION_KEY: &[(&str, &str)] = &[
    ("terrain-raster-descriptor", "raster_descriptor"),
    ("terrain-source-tiles", "terrain_source_tiles"),
    // `manifest-<platform>.json.sig` (ADR-8846): `"format": 1` (ADR-10183: the corpus could not patch it).
    ("release-signature", "format"),
];

/// The byte span of the digits after the JSON key `"key"` (`"key": 12`), for [`JSON_VERSION_KEY`] formats.
fn json_key_span(text: &str, key: &str) -> Option<(usize, usize)> {
    let quoted = format!("\"{key}\"");
    let at = text.find(&quoted)? + quoted.len();
    let rest = &text[at..];
    let colon = rest.len() - rest.trim_start().len();
    let rest = rest.trim_start().strip_prefix(':')?;
    let digits_at = at + colon + 1 + (rest.len() - rest.trim_start().len());
    let digits = text[digits_at..].bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0).then_some((digits_at, digits_at + digits))
}

/// The version a file is at, when it can be read: the first `version` or `format_version` key of a text file
/// (the format's own key for [`JSON_VERSION_KEY`] formats), or the header word of a binary one.
pub fn version_of(format: &str, bytes: &[u8]) -> Option<u32> {
    if let Some(&(_, at, width)) = BINARY_VERSION.iter().find(|(f, _, _)| *f == format) {
        let word = bytes.get(at..at + width)?;
        let mut le = [0u8; 4];
        le[..width].copy_from_slice(word);
        return Some(u32::from_le_bytes(le));
    }
    let text = std::str::from_utf8(bytes).ok()?;
    if let Some(&(_, key)) = JSON_VERSION_KEY.iter().find(|(f, _)| *f == format) {
        return json_key_span(text, key).map(|(a, b)| text[a..b].parse().unwrap_or(0));
    }
    version_span(text).map(|(a, b)| text[a..b].parse().unwrap_or(0))
}

/// `bytes` with its version set to `version`; `None` when the file has no version field to patch.
pub fn with_version(format: &str, bytes: &[u8], version: u32) -> Option<Vec<u8>> {
    if let Some(&(_, at, width)) = BINARY_VERSION.iter().find(|(f, _, _)| *f == format) {
        let mut out = bytes.to_vec();
        out.get_mut(at..at + width)?
            .copy_from_slice(&version.to_le_bytes()[..width]);
        return Some(out);
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let (a, b) = match JSON_VERSION_KEY.iter().find(|(f, _)| *f == format) {
        Some(&(_, key)) => json_key_span(text, key)?,
        None => version_span(text)?,
    };
    Some(format!("{}{version}{}", &text[..a], &text[b..]).into_bytes())
}

/// The byte span of the digits after the first `version:` or `format_version:` key (not in a `//` comment line,
/// not the tail of a longer name such as `scene_format_version`).
fn version_span(text: &str) -> Option<(usize, usize)> {
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        if !line.trim_start().starts_with("//") {
            for key in ["format_version", "version"] {
                let mut from = 0;
                while let Some(i) = line[from..].find(key) {
                    let at = from + i;
                    from = at + key.len();
                    let before = line[..at].chars().next_back();
                    if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                        continue;
                    }
                    let mut i = from;
                    let bytes = line.as_bytes();
                    let skip = |i: &mut usize, f: &dyn Fn(u8) -> bool| {
                        while *i < bytes.len() && f(bytes[*i]) {
                            *i += 1;
                        }
                    };
                    if bytes.get(i) == Some(&b'"') {
                        i += 1;
                    }
                    skip(&mut i, &|c| c == b' ' || c == b'\t');
                    if bytes.get(i) != Some(&b':') && bytes.get(i) != Some(&b'=') {
                        continue;
                    }
                    i += 1;
                    skip(&mut i, &|c| c == b' ' || c == b'\t');
                    let digits_at = i;
                    skip(&mut i, &|c| c.is_ascii_digit());
                    let digits = i - digits_at;
                    if digits > 0 {
                        return Some((line_start + digits_at, line_start + digits_at + digits));
                    }
                }
            }
        }
        line_start += line.len();
    }
    None
}

/// Equal bytes, or text whose parsed RON values are equal (map order may differ).
pub fn same_content(a: &[u8], b: &[u8]) -> bool {
    if a == b {
        return true;
    }
    match (std::str::from_utf8(a), std::str::from_utf8(b)) {
        (Ok(a), Ok(b)) => matches!(
            (ron::from_str::<ron::Value>(a), ron::from_str::<ron::Value>(b)),
            (Ok(x), Ok(y)) if x == y
        ),
        _ => false,
    }
}

/// Whether a refusal message names the version problem.
pub fn names_the_version(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    e.contains("version")
        || e.contains("this build reads")
        || e.contains("toonew")
        || e.contains("newer")
        || e.contains("upgrade")
        || e.contains("too new")
        || e.contains("too_new")
}

/// Whether `REGIA_BLESS_FORMAT_CORPUS` is set: the bless tests then write missing current-version fixtures.
pub fn blessing() -> bool {
    std::env::var_os("REGIA_BLESS_FORMAT_CORPUS").is_some()
}

/// Writes the frozen fixture of `format`'s current `version` unless the corpus has one: `bytes` as `v<version>.<ext>`,
/// plus its `index.ron` entry. Old versions are never made this way (they are real old bytes).
pub fn bless_current(format: &str, version: u32, ext: &str, bytes: &[u8], origin: &str) {
    let have = load_all_indexes();
    let mut entries = have.get(format).cloned().unwrap_or_default();
    if entries.iter().any(|e| e.version == version) {
        return;
    }
    let dir = corpus_dir().join(format);
    std::fs::create_dir_all(&dir).expect("create the format folder");
    let file = format!("v{version}.{ext}");
    std::fs::write(dir.join(&file), bytes).expect("write the fixture");
    entries.push(Entry {
        version,
        file,
        origin: origin.to_string(),
        exercises: "the current version: what the loader writes today must stay readable".into(),
    });
    entries.sort_by_key(|e| e.version);
    let index = Index {
        format: format.to_string(),
        entries,
    };
    let text = ron::ser::to_string_pretty(&index, regia_core::ron_pretty()).expect("encode the index");
    std::fs::write(dir.join("index.ron"), text).expect("write the index");
}

/// Blesses the current version of `format` from its newest older fixture: the loader migrates it and the result,
/// the current writer's output for a real old file, is frozen as `v<current>.<ext>`.
pub fn bless_migrated(format: &str, current: u32, ext: &str, load: Loader<'_>) {
    let have = load_all_indexes();
    let Some(entries) = have.get(format) else { return };
    if entries.iter().any(|e| e.version == current) {
        return;
    }
    let Some(newest) = entries.iter().max_by_key(|e| e.version) else {
        return;
    };
    let bytes = std::fs::read(corpus_dir().join(format).join(&newest.file)).expect("read the older fixture");
    let migrated = load(&bytes).unwrap_or_else(|e| panic!("{format} v{}: {e}", newest.version));
    let origin = format!(
        "the real v{} fixture ({}) migrated by the loader and written at v{current}",
        newest.version, newest.file
    );
    bless_current(format, current, ext, &migrated, &origin);
}

/// A loader under test: parses, migrates to the current version and re-encodes. A panic inside it is a failure.
pub type Loader<'a> = &'a dyn Fn(&[u8]) -> Result<Vec<u8>, String>;

fn guarded(load: Loader<'_>, bytes: &[u8]) -> Result<Vec<u8>, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load(bytes))) {
        Ok(r) => r,
        Err(_) => Err("panic: the loader panicked".to_string()),
    }
}

/// Runs the four checks over every corpus fixture of `format` and returns the problems (empty when all pass;
/// also a problem when the format has no fixtures at all, unless `allow_empty`).
///
/// The checks:
///
/// 1. the fixture loads (and its result is never above `current`);
/// 2. the re-encoding loads again and reads back equal;
/// 3. migrating is idempotent;
/// 4. a copy whose version is patched to `current + 1` is refused with an error that names the version, never a
///    panic and never a silent load.
pub fn check_format(format: &str, current: u32, load: Loader<'_>) -> Vec<String> {
    check_format_with(format, current, load, &|b| version_of(format, b), &|b, v| {
        with_version(format, b, v)
    })
}

/// The checks a loader that does not re-encode allows: every fixture loads, and a copy patched to `current + 1`
/// is refused with an error that names the version.
pub fn check_load_only(format: &str, current: u32, load: Loader<'_>) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(entries) = load_all_indexes().remove(format) else {
        return vec![format!("{format}: no corpus folder")];
    };
    for entry in &entries {
        let tag = format!("{format} v{}", entry.version);
        let path = corpus_dir().join(format).join(&entry.file);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                problems.push(format!("{tag}: {}: {e}", path.display()));
                continue;
            }
        };
        if let Err(e) = guarded(load, &bytes) {
            problems.push(format!("{tag}: does not load: {e}"));
            continue;
        }
        match with_version(format, &bytes, current + 1) {
            None if NO_VERSION_FIELD.iter().any(|(f, _)| *f == format) => {}
            None => problems.push(format!(
                "{tag}: no way to patch the version field (see BINARY_VERSION)"
            )),
            Some(future) => match guarded(load, &future) {
                Ok(_) => problems.push(format!(
                    "{tag}: a copy at version {} loaded silently",
                    current + 1
                )),
                Err(e) if e.starts_with("panic:") || e.is_empty() => {
                    problems.push(format!("{tag}: future version: {e:?}"))
                }
                Err(e) if names_the_version(&e) => {}
                Err(e)
                    if FUTURE_OK_ERRORS
                        .iter()
                        .any(|(f, m)| *f == format && e.contains(m)) => {}
                Err(e) => problems.push(format!(
                    "{tag}: future version refused, but the error does not name it: {e}"
                )),
            },
        }
    }
    problems
}

/// Reads the version of a file for a format whose version is not a text key or a [`BINARY_VERSION`] header word.
pub type VersionReader<'a> = &'a dyn Fn(&[u8]) -> Option<u32>;

/// Sets the version of a file, for a format that stores it in a place only its own crate can patch (a cooked
/// payload behind a header). It is given the *migrated* output of the loader when the fixture itself cannot be
/// patched.
pub type VersionPatcher<'a> = &'a dyn Fn(&[u8], u32) -> Option<Vec<u8>>;

/// [`check_format`] with the version reader and patcher of the format's crate. The future-version copy is made
/// from the fixture when `patch` can patch it, from the loader's output otherwise.
pub fn check_format_with(
    format: &str,
    current: u32,
    load: Loader<'_>,
    read: VersionReader<'_>,
    patch: VersionPatcher<'_>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(entries) = load_all_indexes().remove(format) else {
        return vec![format!("{format}: no corpus folder")];
    };
    for entry in &entries {
        let path = corpus_dir().join(format).join(&entry.file);
        let tag = format!("{format} v{}", entry.version);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                problems.push(format!("{tag}: {}: {e}", path.display()));
                continue;
            }
        };
        let migrated = match guarded(load, &bytes) {
            Ok(m) => m,
            Err(e) => {
                problems.push(format!("{tag}: does not load: {e}"));
                continue;
            }
        };
        // The loader's output is this build's canonical form of the file: it must be stamped at the current
        // version. A loader that only parses (it accepts the older layout and keeps its stored number) is
        // named in PARSE_ONLY_LOADERS, with "reaches the current version" asserted by format_corpus_upgrade
        // through `regia upgrade-project`; a number above the current one is always wrong.
        match read(&migrated) {
            Some(v) if v > current => problems.push(format!(
                "{tag}: migrated to version {v}, above the current {current}"
            )),
            Some(v) if v != current && !PARSE_ONLY_LOADERS.iter().any(|(f, _)| *f == format) => problems
                .push(format!(
                    "{tag}: the loader left the output at version {v}, not the current {current} \
                     (fix the writer, or list the format in PARSE_ONLY_LOADERS with the \
                     format_corpus_upgrade check as the reason)"
                )),
            _ => {}
        }
        match guarded(load, &migrated) {
            Err(e) => problems.push(format!("{tag}: the migrated output no longer loads: {e}")),
            Ok(again) if !same_content(&migrated, &again) => {
                problems.push(format!(
                    "{tag}: migrating twice changed the output (not idempotent)"
                ));
            }
            Ok(_) => {}
        }
        match patch(&bytes, current + 1).or_else(|| patch(&migrated, current + 1)) {
            None if NO_VERSION_FIELD.iter().any(|(f, _)| *f == format) => {}
            None => problems.push(format!(
                "{tag}: no way to patch the version field (see BINARY_VERSION)"
            )),
            Some(future) => match guarded(load, &future) {
                Ok(_) => problems.push(format!(
                    "{tag}: a copy at version {} loaded silently",
                    current + 1
                )),
                Err(e) if e.starts_with("panic:") => problems.push(format!("{tag}: future version: {e}")),
                Err(e) if e.is_empty() => {
                    problems.push(format!("{tag}: future version refused with an empty error"))
                }
                Err(e) if names_the_version(&e) => {}
                Err(e)
                    if FUTURE_OK_ERRORS
                        .iter()
                        .any(|(f, m)| *f == format && e.contains(m)) => {}
                Err(e) => problems.push(format!(
                    "{tag}: future version refused, but the error does not name it: {e}"
                )),
            },
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_patches_named_json_version_keys() {
        let text = br#"{"raster_descriptor": 1, "spacing_m": [1.0, 1.0]}"#;
        assert_eq!(version_of("terrain-raster-descriptor", text), Some(1));
        let patched = with_version("terrain-raster-descriptor", text, 2).expect("patch");
        assert_eq!(patched, br#"{"raster_descriptor": 2, "spacing_m": [1.0, 1.0]}"#);
        assert_eq!(
            version_of("terrain-source-tiles", br#"{"terrain_source_tiles":17}"#),
            Some(17)
        );
        assert_eq!(version_of("terrain-source-tiles", br#"{"tiles": []}"#), None);
    }

    #[test]
    fn reads_and_patches_text_versions() {
        let t = b"// version: 9\n(\n  scene_format_version: 10,\n  format_version: 3,\n)";
        assert_eq!(version_of("x", t), Some(3));
        let p = with_version("x", t, 4).expect("patches");
        assert_eq!(version_of("x", &p), Some(4));
        assert!(
            String::from_utf8(p)
                .expect("UTF-8")
                .contains("scene_format_version: 10")
        );
        assert_eq!(version_of("x", b"(a: 1)"), None);
        assert_eq!(version_of("x", br#"{"version": 12}"#), Some(12));
    }

    #[test]
    fn patches_a_binary_header() {
        let b = [b'F', b'L', b'M', b'P', 5, 0, 0, 0, 9];
        assert_eq!(version_of("lightmap", &b), Some(5));
        assert_eq!(with_version("lightmap", &b, 6).expect("patches")[4], 6);
    }
}
