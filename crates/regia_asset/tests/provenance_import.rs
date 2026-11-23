// SPDX-License-Identifier: MPL-2.0
//! Pack provenance records into `.meta` provenance (ADR-8834, plan row C-24): the acceptance
//! tests. A voice row keeps every field it carries, an unknown field is kept (or refused when it
//! cannot be an extension key), a duplicate row is reported, and a second run writes nothing.

use std::path::Path;

use regia_asset::provenance_import::{
    CODE_PROVENANCE_IMPORT_CONFLICT, CODE_PROVENANCE_IMPORT_INVALID, CONFLICT_KINDS, ImportOptions,
    PROVENANCE_IMPORT_CODES, PackRecord, ProvenanceImportError, file_sha256_hex, import_provenance,
    read_pack_record, record_files,
};
use regia_asset::{AssetId, MetaSidecar, Provenance, ProvenanceTool, ReviewerState, meta_path_for};
use regia_test_utils::TempDir;

/// A project with one source under `assets/` and its `.meta` (no provenance); returns the
/// source's SHA-256 and length.
fn put_source(assets: &Path, rel: &str, bytes: &[u8], importer: &str) -> (String, u64) {
    let source = assets.join(rel);
    std::fs::create_dir_all(source.parent().expect("parent")).expect("dir");
    std::fs::write(&source, bytes).expect("source");
    let meta = MetaSidecar::new(importer, AssetId::new_random());
    std::fs::write(meta_path_for(&source), meta.to_ron_string().expect("ron")).expect("meta");
    (file_sha256_hex(&source).expect("hash"), bytes.len() as u64)
}

fn read_meta(assets: &Path, rel: &str) -> MetaSidecar {
    let bytes = std::fs::read(meta_path_for(&assets.join(rel))).expect("read meta");
    MetaSidecar::from_ron(&bytes).expect("parse meta")
}

fn meta_bytes(assets: &Path, rel: &str) -> Vec<u8> {
    std::fs::read(meta_path_for(&assets.join(rel))).expect("read meta")
}

/// A record text with one generated mesh row.
fn mesh_row(path: &str, sha: &str, len: u64, extra: &str) -> String {
    format!(
        r#"
[[files]]
path = "{path}"
sha256 = "{sha}"
byte_len = {len}
origin = "original"
tool = "Example Model"
generator = "tools/gen/mesh.py"
seed = 0
created_utc = "2026-09-28T00:00:00Z"
reviewer = "owner"
foreign_game_file = false
third_party_mod_file = false
{extra}
"#
    )
}

fn record(source: &str, pack: &str, rows: &str) -> PackRecord {
    let text = format!("schema = 3\npack_id = \"{pack}\"\nstatus = \"approved\"\n{rows}");
    PackRecord::parse(source, &text).unwrap_or_else(|e| panic!("{source}: {e}"))
}

fn import(
    assets: &Path,
    records: &[PackRecord],
) -> Result<regia_asset::provenance_import::ImportReport, ProvenanceImportError> {
    import_provenance(assets, records, &ImportOptions::default())
}

#[test]
fn voice_row_survives_migration() {
    let dir = TempDir::new("prov-import-voice");
    let assets = dir.join("assets");
    let (sha, len) = put_source(&assets, "voice/round_over_1.ogg", b"OggS voice take", "ogg");
    let rows = format!(
        r#"
[[files]]
path = "voice/round_over_1.ogg"
sha256 = "{sha}"
byte_len = {len}
origin = "original"
tool = "Narrator voice"
generator = "tools/gen/voice_bank.py"
seed = 7
line_id = "match.round_over"
variant = 1
text = "Round over. {{team}} wins."
voice = "narrator"
tts = "example-tts"
take_approved_utc = 2026-09-29T10:00:00Z
created_utc = "2026-09-28T00:00:00Z"
reviewer = "owner"
foreign_game_file = false
third_party_mod_file = false
"#
    );
    let rec = record("provenance/voice.toml", "voice", &rows);
    let report = import(&assets, &[rec]).expect("import");
    assert_eq!(report.written, ["voice/round_over_1.ogg"]);
    assert!(report.applied);

    let meta = read_meta(&assets, "voice/round_over_1.ogg");
    assert_eq!(meta.importer, "ogg", "the rest of the .meta is kept");
    let p = meta.provenance.expect("provenance block");
    assert_eq!(p.tool, ProvenanceTool::Generator);
    assert_eq!(p.generator.as_deref(), Some("tools/gen/voice_bank.py"));
    assert_eq!(p.seed, Some(7));
    assert_eq!(p.origin.as_deref(), Some("original"));
    assert_eq!(p.created_utc.as_deref(), Some("2026-09-28T00:00:00Z"));
    assert_eq!(p.reviewer, ReviewerState::Owner);
    let ext = |k: &str| p.extensions.get(k).map(String::as_str);
    // The voice-row extension fields, each under its own name.
    assert_eq!(ext("line_id"), Some("match.round_over"));
    assert_eq!(ext("variant"), Some("1"));
    assert_eq!(ext("text"), Some("Round over. {team} wins."));
    assert_eq!(ext("voice"), Some("narrator"));
    assert_eq!(ext("tts"), Some("example-tts"));
    assert_eq!(ext("take_approved_utc"), Some("2026-09-29T10:00:00Z"));
    // The base row fields the block has no slot for.
    assert_eq!(ext("sha256"), Some(sha.as_str()));
    assert_eq!(ext("byte_len"), Some(len.to_string().as_str()));
    assert_eq!(ext("source_tool"), Some("Narrator voice"));
    assert_eq!(ext("foreign_game_file"), Some("false"));
    assert_eq!(ext("third_party_mod_file"), Some("false"));
    assert_eq!(ext("pack_id"), Some("voice"));
    assert_eq!(ext("pack_status"), Some("approved"));
    assert_eq!(p.extensions.len(), 13, "{:?}", p.extensions);
}

#[test]
fn unknown_fields_are_kept_and_unkeepable_ones_refused() {
    let dir = TempDir::new("prov-import-unknown");
    let assets = dir.join("assets");
    let (sha, len) = put_source(&assets, "mesh/crate.obj", b"v 0 0 0\n", "obj");
    let extra =
        "licence = \"CC0-1.0\"\ntags = [\"prop\", \"wood\"]\nweight = 1.5\nmodel = \"example-model-2\"";
    let text = format!(
        "schema = 3\npack_id = \"base\"\nstatus = \"quarantine\"\nnotes = \"kept on every row\"\n{}",
        mesh_row("mesh/crate.obj", &sha, len, extra)
    );
    let rec = PackRecord::parse("provenance/base.toml", &text).expect("parse");
    import(&assets, &[rec]).expect("import");
    let p = read_meta(&assets, "mesh/crate.obj").provenance.expect("block");
    let ext = |k: &str| p.extensions.get(k).map(String::as_str);
    assert_eq!(ext("licence"), Some("CC0-1.0"));
    assert_eq!(ext("tags"), Some(r#"["prop", "wood"]"#));
    assert_eq!(ext("weight"), Some("1.5"));
    assert_eq!(ext("pack_notes"), Some("kept on every row"));
    assert_eq!(ext("pack_status"), Some("quarantine"));
    assert_eq!(
        p.model.as_deref(),
        Some("example-model-2"),
        "`model` fills the built-in field"
    );
    assert_eq!(ext("model"), None);

    // Refused: names that cannot be extension keys, keys the import sets, and tables.
    let sha0 = "0".repeat(64);
    for (extra, pack_extra, needle) in [
        ("Bad-Key = \"x\"", "", "cannot be kept"),
        ("source_tool = \"x\"", "", "would overwrite"),
        ("pack_owner = \"x\"", "", "would overwrite"),
        ("details = { a = 1 }", "", "is a table"),
        ("", "id = \"other\"", "would overwrite"),
        ("", "Notes = \"x\"", "cannot be kept"),
    ] {
        let text = format!(
            "schema = 3\npack_id = \"p\"\nstatus = \"approved\"\n{pack_extra}\n{}",
            mesh_row("a.obj", &sha0, 1, extra)
        );
        let err = PackRecord::parse("provenance/bad.toml", &text).expect_err(extra);
        assert_eq!(err.code(), CODE_PROVENANCE_IMPORT_INVALID);
        assert!(err.to_string().contains(needle), "{extra} {pack_extra}: {err}");
    }
}

#[test]
fn duplicate_row_is_reported() {
    let dir = TempDir::new("prov-import-dup");
    let assets = dir.join("assets");
    let (sha, len) = put_source(&assets, "mesh/rock.obj", b"v 1 2 3\n", "obj");
    let row = mesh_row("mesh/rock.obj", &sha, len, "");
    let a = record("provenance/a.toml", "shared", &row);
    let b = record("provenance/b.toml", "shared", &row);
    let report = import(&assets, &[a, b]).expect("identical duplicates migrate once");
    assert_eq!(report.written, ["mesh/rock.obj"]);
    assert_eq!(report.duplicates.len(), 1);
    assert_eq!(report.duplicates[0].path, "mesh/rock.obj");
    assert_eq!(
        report.duplicates[0].records,
        ["provenance/a.toml", "provenance/b.toml"]
    );
    assert!(report.duplicates[0].identical);

    // Rows that differ (another pack id), and a spelling in another case, are conflicts: nothing
    // is written.
    let (sha2, len2) = put_source(&assets, "mesh/tree.obj", b"v 4 5 6\n", "obj");
    let before = meta_bytes(&assets, "mesh/tree.obj");
    let c = record(
        "provenance/c.toml",
        "one",
        &mesh_row("mesh/tree.obj", &sha2, len2, ""),
    );
    let d = record(
        "provenance/d.toml",
        "two",
        &mesh_row("Mesh/Tree.obj", &sha2, len2, ""),
    );
    let Err(ProvenanceImportError::Conflict(report)) = import(&assets, &[c, d]) else {
        panic!("differing duplicates must conflict");
    };
    assert_eq!(report.duplicates.len(), 1);
    assert!(!report.duplicates[0].identical);
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].kind, "duplicate_differs");
    assert!(!report.applied);
    assert_eq!(meta_bytes(&assets, "mesh/tree.obj"), before, "nothing written");

    // A record that lists one path twice is a duplicate too.
    let twice = record("provenance/e.toml", "shared", &format!("{row}{row}"));
    let report = import(&assets, &[twice]).expect("already migrated");
    assert_eq!(
        report.duplicates[0].records,
        ["provenance/e.toml", "provenance/e.toml"]
    );
}

#[test]
fn migration_is_idempotent() {
    let dir = TempDir::new("prov-import-idem");
    let assets = dir.join("assets");
    let (sha1, len1) = put_source(&assets, "mesh/a.obj", b"a", "obj");
    let (sha2, len2) = put_source(&assets, "tex/b.png", b"bb", "png");
    let rows = format!(
        "{}{}",
        mesh_row("mesh/a.obj", &sha1, len1, "seed_note = \"x\""),
        mesh_row("tex/b.png", &sha2, len2, "")
    );
    let rec = record("provenance/base.toml", "base", &rows);
    let first = import(&assets, std::slice::from_ref(&rec)).expect("first");
    assert_eq!(first.written, ["mesh/a.obj", "tex/b.png"]);
    let after_first = (
        meta_bytes(&assets, "mesh/a.obj"),
        meta_bytes(&assets, "tex/b.png"),
    );
    let second = import(&assets, &[rec]).expect("second");
    assert!(second.written.is_empty(), "{second:?}");
    assert_eq!(second.kept, ["mesh/a.obj", "tex/b.png"]);
    let after_second = (
        meta_bytes(&assets, "mesh/a.obj"),
        meta_bytes(&assets, "tex/b.png"),
    );
    assert_eq!(after_first, after_second, "a second run gives the same bytes");
}

#[test]
fn signed_seeds_are_kept_exactly() {
    let dir = TempDir::new("prov-import-seed");
    let assets = dir.join("assets");
    let (sha, len) = put_source(&assets, "a.obj", b"neg", "obj");
    let (sha2, len2) = put_source(&assets, "b.obj", b"max", "obj");
    let neg = mesh_row("a.obj", &sha, len, "").replace("seed = 0", "seed = -5");
    let max = mesh_row("b.obj", &sha2, len2, "").replace("seed = 0", &format!("seed = {}", i64::MAX));
    import(&assets, &[record("p.toml", "p", &format!("{neg}{max}"))]).expect("import");
    let a = read_meta(&assets, "a.obj").provenance.expect("a");
    assert_eq!(a.seed, None);
    assert_eq!(a.extensions.get("seed_signed").map(String::as_str), Some("-5"));
    let b = read_meta(&assets, "b.obj").provenance.expect("b");
    assert_eq!(b.seed, Some(i64::MAX as u64));
    assert!(!b.extensions.contains_key("seed_signed"));
}

#[test]
fn hand_rows_reviewers_and_tools_map_to_the_block() {
    let dir = TempDir::new("prov-import-map");
    let assets = dir.join("assets");
    let (sha, len) = put_source(&assets, "hand.obj", b"hand", "obj");
    let (sha2, len2) = put_source(&assets, "rec.ogg", b"rec", "ogg");
    let hand = mesh_row("hand.obj", &sha, len, "")
        .replace("tool = \"Example Model\"", "tool = \"hand\"")
        .replace("generator = \"tools/gen/mesh.py\"", "generator = \"none\"")
        .replace("seed = 0\n", "")
        .replace("reviewer = \"owner\"", "reviewer = \"\"");
    let rec = mesh_row("rec.ogg", &sha2, len2, "")
        .replace("generator = \"tools/gen/mesh.py\"", "generator = \"none\"")
        .replace("reviewer = \"owner\"", "reviewer = \"ana\"");
    import(&assets, &[record("p.toml", "p", &format!("{hand}{rec}"))]).expect("import");
    let h = read_meta(&assets, "hand.obj").provenance.expect("hand");
    assert_eq!(h.tool, ProvenanceTool::Hand);
    assert_eq!(h.generator, None);
    assert_eq!(h.seed, None);
    assert_eq!(h.reviewer, ReviewerState::Pending);
    let r = read_meta(&assets, "rec.ogg").provenance.expect("rec");
    assert_eq!(
        r.tool,
        ProvenanceTool::Import,
        "no generator and not hand: imported"
    );
    assert_eq!(r.reviewer, ReviewerState::Named("ana".into()));
    assert_eq!(
        r.extensions.get("source_tool").map(String::as_str),
        Some("Example Model")
    );
}

#[test]
fn conflicts_write_nothing() {
    let dir = TempDir::new("prov-import-conflict");
    let assets = dir.join("assets");
    let (sha, len) = put_source(&assets, "good.obj", b"good", "obj");
    let (_, len_bad) = put_source(&assets, "stale.obj", b"stale", "obj");
    put_source(&assets, "short.obj", b"short", "obj");
    let (sha_owned, len_owned) = put_source(&assets, "owned.obj", b"owned", "obj");
    put_source(&assets, "broken.obj", b"broken", "obj");
    std::fs::write(meta_path_for(&assets.join("broken.obj")), "not ron").expect("broken meta");
    let (sha_broken, len_broken) = (file_sha256_hex(&assets.join("broken.obj")).expect("h"), 6);
    // `owned.obj` already has another block.
    let mut owned = read_meta(&assets, "owned.obj");
    owned.provenance = Some(Provenance {
        tool: ProvenanceTool::Agent,
        ..Provenance::default()
    });
    std::fs::write(
        meta_path_for(&assets.join("owned.obj")),
        owned.to_ron_string().expect("ron"),
    )
    .expect("owned meta");
    let before: Vec<Vec<u8>> = ["good.obj", "stale.obj", "short.obj", "owned.obj"]
        .iter()
        .map(|p| meta_bytes(&assets, p))
        .collect();
    let rows = [
        mesh_row("good.obj", &sha, len, ""),
        mesh_row("stale.obj", &"ab".repeat(32), len_bad, ""),
        mesh_row("short.obj", &"cd".repeat(32), 99, ""),
        mesh_row("owned.obj", &sha_owned, len_owned, ""),
        mesh_row("broken.obj", &sha_broken, len_broken, ""),
    ]
    .concat();
    let Err(err) = import(&assets, &[record("p.toml", "p", &rows)]) else {
        panic!("conflicts must refuse the import");
    };
    assert_eq!(err.code(), CODE_PROVENANCE_IMPORT_CONFLICT);
    let ProvenanceImportError::Conflict(report) = err else {
        unreachable!()
    };
    let kinds: Vec<(&str, &str)> = report
        .conflicts
        .iter()
        .map(|c| (c.path.as_str(), c.kind.as_str()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("broken.obj", "meta_unreadable"),
            ("owned.obj", "provenance_differs"),
            ("short.obj", "byte_len_mismatch"),
            ("stale.obj", "hash_mismatch"),
        ]
    );
    assert!(
        report
            .conflicts
            .iter()
            .all(|c| CONFLICT_KINDS.contains(&c.kind.as_str()))
    );
    assert_eq!(report.written, ["good.obj"], "what would have been written");
    assert!(!report.applied);
    let after: Vec<Vec<u8>> = ["good.obj", "stale.obj", "short.obj", "owned.obj"]
        .iter()
        .map(|p| meta_bytes(&assets, p))
        .collect();
    assert_eq!(before, after, "nothing was written");
}

#[test]
fn missing_sources_and_sidecars_are_skipped_and_dry_run_writes_nothing() {
    let dir = TempDir::new("prov-import-missing");
    let assets = dir.join("assets");
    let (sha, len) = put_source(&assets, "packs/base/mesh/a.obj", b"a", "obj");
    std::fs::write(assets.join("packs/base/mesh/nometa.obj"), b"n").expect("src");
    let sha_n = file_sha256_hex(&assets.join("packs/base/mesh/nometa.obj")).expect("h");
    let rows = [
        mesh_row("mesh/a.obj", &sha, len, ""),
        mesh_row("mesh/gone.obj", &sha, len, ""),
        mesh_row("mesh/nometa.obj", &sha_n, 1, ""),
    ]
    .concat();
    let rec = record("p.toml", "base", &rows);
    let opts = ImportOptions {
        under: Some("packs/base".into()),
        dry_run: true,
    };
    let before = meta_bytes(&assets, "packs/base/mesh/a.obj");
    let dry = import_provenance(&assets, std::slice::from_ref(&rec), &opts).expect("dry run");
    assert!(!dry.applied);
    assert_eq!(dry.written, ["packs/base/mesh/a.obj"]);
    assert_eq!(dry.missing_source, ["packs/base/mesh/gone.obj"]);
    assert_eq!(dry.missing_meta, ["packs/base/mesh/nometa.obj"]);
    assert_eq!(dry.records, 1);
    assert_eq!(dry.rows, 3);
    assert_eq!(
        meta_bytes(&assets, "packs/base/mesh/a.obj"),
        before,
        "a dry run writes nothing"
    );
    let opts = ImportOptions {
        dry_run: false,
        ..opts
    };
    let real = import_provenance(&assets, &[rec], &opts).expect("import");
    assert!(real.applied);
    assert_eq!(real.written, dry.written);
    assert!(
        !assets.join("packs/base/mesh/nometa.obj.meta").exists(),
        "no sidecar is created"
    );
}

#[test]
fn unsafe_paths_and_bad_records_are_refused() {
    let sha0 = "0".repeat(64);
    for path in [
        "../escape.obj",
        "/abs.obj",
        "a//b.obj",
        "a\\b.obj",
        "C:/x.obj",
        "a.obj.meta",
        "",
    ] {
        let text = format!(
            "schema = 3\npack_id = \"p\"\nstatus = \"approved\"\n{}",
            mesh_row(path, &sha0, 1, "")
        );
        let err = PackRecord::parse("p.toml", &text).expect_err(path);
        assert_eq!(err.code(), CODE_PROVENANCE_IMPORT_INVALID, "{path}");
    }
    let row = mesh_row("a.obj", &sha0, 1, "");
    for (text, needle) in [
        (
            format!("schema = 4\npack_id = \"p\"\nstatus = \"a\"\n{row}"),
            "not supported",
        ),
        (
            format!("pack_id = \"p\"\nstatus = \"a\"\n{row}"),
            "`schema` is missing",
        ),
        (
            format!("schema = 3\npack_id = \"\"\nstatus = \"a\"\n{row}"),
            "empty",
        ),
        (
            format!(
                "schema = 3\npack_id = \"p\"\nstatus = \"a\"\n{}",
                row.replace(&sha0, "xyz")
            ),
            "64 hex",
        ),
        (
            format!(
                "schema = 3\npack_id = \"p\"\nstatus = \"a\"\n{}",
                row.replace("byte_len = 1", "byte_len = -1")
            ),
            "non-negative",
        ),
        (
            format!(
                "schema = 3\npack_id = \"p\"\nstatus = \"a\"\n{}",
                row.replace("seed = 0", "seed = \"0\"")
            ),
            "`seed` must be an integer",
        ),
        (
            format!(
                "schema = 3\npack_id = \"p\"\nstatus = \"a\"\n{}",
                row.replace("reviewer = \"owner\"\n", "")
            ),
            "`reviewer` is missing",
        ),
        (
            "schema = 3\npack_id = \"p\"\nstatus = \"a\"\nfiles = 1".to_string(),
            "array of tables",
        ),
        ("not = = toml".to_string(), "not a TOML record"),
    ] {
        let err = PackRecord::parse("p.toml", &text).expect_err(needle);
        assert_eq!(err.code(), CODE_PROVENANCE_IMPORT_INVALID);
        assert!(err.to_string().contains(needle), "{needle}: {err}");
    }
    // An unsafe `under` is refused before anything is read.
    let dir = TempDir::new("prov-import-under");
    let opts = ImportOptions {
        under: Some("../outside".into()),
        dry_run: false,
    };
    let err = import_provenance(&dir.join("assets"), &[], &opts).expect_err("under");
    assert_eq!(err.code(), CODE_PROVENANCE_IMPORT_INVALID);
}

#[test]
fn record_folders_read_sorted_and_truncated_records_never_panic() {
    let dir = TempDir::new("prov-import-files");
    let folder = dir.join("provenance");
    std::fs::create_dir_all(&folder).expect("dir");
    let sha0 = "0".repeat(64);
    let text = format!(
        "schema = 3\npack_id = \"b\"\nstatus = \"approved\"\n{}",
        mesh_row("a.obj", &sha0, 1, "line_id = \"x\"")
    );
    std::fs::write(folder.join("b.toml"), &text).expect("b");
    std::fs::write(
        folder.join("a.toml"),
        "schema = 3\npack_id = \"a\"\nstatus = \"approved\"\nfiles = []\n",
    )
    .expect("a");
    std::fs::write(folder.join("notes.txt"), "not a record").expect("txt");
    let files = record_files(&folder).expect("list");
    let names: Vec<String> = files
        .iter()
        .map(|f| f.file_name().expect("name").to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["a.toml", "b.toml"]);
    let b = read_pack_record(&folder.join("b.toml"), "provenance/b.toml").expect("read");
    assert_eq!(b.rows.len(), 1);
    assert_eq!(b.source, "provenance/b.toml");
    assert_eq!(
        record_files(&folder.join("b.toml")).expect("one"),
        [folder.join("b.toml")]
    );
    let err = read_pack_record(&folder.join("missing.toml"), "x").expect_err("missing");
    assert_eq!(err.code(), "provenance_import_unreadable");
    // Every prefix of a valid record parses or is refused; none panics.
    for end in (0..=text.len()).filter(|&i| text.is_char_boundary(i)) {
        let _ = PackRecord::parse("cut.toml", &text[..end]);
    }
}

#[test]
fn every_code_is_lowercase() {
    for code in PROVENANCE_IMPORT_CODES.iter().chain(CONFLICT_KINDS) {
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{code}"
        );
    }
}

/// Review proof: `check_rel_path` is narrower than `regia_core::project_path::check_relative`.
/// A Windows device name, a parent segment with a trailing space, a dot-only segment and a
/// trailing dot are hostile paths. They must be `provenance_import_invalid`.
#[test]
fn device_names_and_dotted_parents_are_refused() {
    let sha0 = "0".repeat(64);
    for path in [
        "NUL",
        "mesh/COM1.obj",
        "aux.obj",
        ".. /x.obj",
        "foo/.../x.obj",
        "bar.obj.",
    ] {
        let text = format!(
            "schema = 3\npack_id = \"p\"\nstatus = \"approved\"\n{}",
            mesh_row(path, &sha0, 1, "")
        );
        let err = PackRecord::parse("p.toml", &text).expect_err(path);
        assert_eq!(err.code(), CODE_PROVENANCE_IMPORT_INVALID, "{path}: {err}");
    }
    let dir = TempDir::new("prov-import-under-hostile");
    for under in ["NUL", ".. /x", "foo/.../x", "COM1"] {
        let opts = ImportOptions {
            under: Some(under.to_string()),
            dry_run: true,
        };
        let err = import_provenance(&dir.join("assets"), &[], &opts).expect_err(under);
        assert_eq!(err.code(), CODE_PROVENANCE_IMPORT_INVALID, "{under}: {err}");
    }
}

/// Review proof: a record folder must not skip hostile or indirect `*.toml` names.
/// A FIFO is `provenance_import_unreadable` (the direct-path rule). A symlink to a regular
/// file inside the folder is a record and is imported.
#[cfg(unix)]
#[test]
fn a_record_folder_refuses_a_fifo_and_reads_a_symlink() {
    let dir = TempDir::new("prov-import-folder-hostile");
    let folder = dir.join("provenance");
    std::fs::create_dir_all(&folder).expect("dir");
    let fifo = folder.join("pack.toml");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(made.success(), "mkfifo");
    let err = record_files(&folder).expect_err("a fifo named *.toml is refused");
    assert_eq!(err.code(), "provenance_import_unreadable");
    std::fs::remove_file(&fifo).expect("remove fifo");

    let assets = dir.join("assets");
    let (sha, len) = put_source(&assets, "a.obj", b"obj", "obj");
    let text = format!(
        "schema = 3\npack_id = \"p\"\nstatus = \"approved\"\n{}",
        mesh_row("a.obj", &sha, len, "")
    );
    std::fs::write(folder.join("payload"), &text).expect("payload");
    std::os::unix::fs::symlink(folder.join("payload"), folder.join("p.toml")).expect("link");
    let files = record_files(&folder).expect("list");
    assert!(
        files.iter().any(|p| p.ends_with("p.toml")),
        "symlink record was dropped: {files:?}"
    );
    let rec = read_pack_record(
        files.iter().find(|p| p.ends_with("p.toml")).expect("p"),
        "provenance/p.toml",
    )
    .expect("read");
    let report = import(&assets, &[rec]).expect("import");
    assert_eq!(report.written, ["a.obj"]);
}

/// Review proof: a conflict discovered while writing (`meta_changed`) writes nothing, and a
/// later write error does not leave the earlier sidecars changed.
#[cfg(unix)]
#[test]
fn a_conflict_or_write_error_during_the_write_leaves_nothing_changed() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = TempDir::new("prov-import-atomic");
    let assets = dir.join("assets");
    let (sha_a, len_a) = put_source(&assets, "a.obj", b"a", "obj");
    let (sha_b, len_b) = put_source(&assets, "b.obj", b"b", "obj");
    let before = meta_bytes(&assets, "a.obj");
    let meta_b = meta_path_for(&assets.join("b.obj"));
    std::fs::remove_file(&meta_b).expect("remove b meta");
    std::os::unix::fs::symlink(meta_path_for(&assets.join("a.obj")), &meta_b).expect("link metas");
    let rows = format!(
        "{}{}",
        mesh_row("a.obj", &sha_a, len_a, ""),
        mesh_row("b.obj", &sha_b, len_b, "").replace("seed = 0", "seed = 1")
    );
    let err = import(&assets, &[record("p.toml", "p", &rows)]).expect_err("meta_changed");
    assert_eq!(err.code(), CODE_PROVENANCE_IMPORT_CONFLICT);
    let ProvenanceImportError::Conflict(report) = err else {
        unreachable!()
    };
    assert!(
        report.conflicts.iter().any(|c| c.kind == "meta_changed"),
        "{report:?}"
    );
    assert!(!report.applied, "a conflict is not applied: {report:?}");
    assert_eq!(
        meta_bytes(&assets, "a.obj"),
        before,
        "meta_changed must not leave the earlier sidecar written"
    );

    let (sha_c, len_c) = put_source(&assets, "c.obj", b"c", "obj");
    let (sha_d, len_d) = put_source(&assets, "z/d.obj", b"d", "obj");
    let before_c = meta_bytes(&assets, "c.obj");
    let z = assets.join("z");
    let mut perms = std::fs::metadata(&z).expect("z").permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(&z, perms).expect("chmod");
    let rows = format!(
        "{}{}",
        mesh_row("c.obj", &sha_c, len_c, ""),
        mesh_row("z/d.obj", &sha_d, len_d, "")
    );
    let result = import(&assets, &[record("q.toml", "q", &rows)]);
    let mut perms = std::fs::metadata(&z).expect("z").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&z, perms).expect("restore");
    let err = result.expect_err("second write fails");
    assert_eq!(err.code(), "provenance_import_write_failed");
    assert_eq!(
        meta_bytes(&assets, "c.obj"),
        before_c,
        "a write failure must not leave the earlier sidecar written"
    );
}

/// Review proof: a directory symlink that leaves `assets/` must not rewrite a sidecar outside it.
#[cfg(unix)]
#[test]
fn a_symlink_out_of_assets_is_refused_and_writes_nothing() {
    let dir = TempDir::new("prov-import-symlink-out");
    let assets = dir.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    let outside = dir.join("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    let secret = outside.join("secret.obj");
    std::fs::write(&secret, b"secret").expect("secret");
    let outside_meta = meta_path_for(&secret);
    std::fs::write(
        &outside_meta,
        MetaSidecar::new("obj", AssetId::new_random())
            .to_ron_string()
            .expect("ron"),
    )
    .expect("outside meta");
    let outside_before = std::fs::read(&outside_meta).expect("read outside");
    std::os::unix::fs::symlink(&outside, assets.join("out")).expect("link");
    let (sha_ok, len_ok) = put_source(&assets, "ok.obj", b"ok", "obj");
    let before_ok = meta_bytes(&assets, "ok.obj");
    let sha = file_sha256_hex(&secret).expect("hash");
    let rows = format!(
        "{}{}",
        mesh_row("ok.obj", &sha_ok, len_ok, ""),
        mesh_row("out/secret.obj", &sha, 6, "")
    );
    let err = import(&assets, &[record("p.toml", "p", &rows)]).expect_err("escape");
    assert_eq!(err.code(), CODE_PROVENANCE_IMPORT_INVALID, "{err}");
    assert_eq!(
        std::fs::read(&outside_meta).expect("outside still there"),
        outside_before,
        "a sidecar outside assets/ was rewritten"
    );
    assert_eq!(meta_bytes(&assets, "ok.obj"), before_ok, "nothing was written");
}

/// Fix lane: a `.meta` that is a symbolic link is `meta_link`; the link and its target keep
/// their bytes, and the other rows are not written either.
#[cfg(unix)]
#[test]
fn a_linked_meta_is_refused_and_keeps_the_link() {
    let dir = TempDir::new("prov-import-meta-link");
    let assets = dir.join("assets");
    let (sha_a, len_a) = put_source(&assets, "a.obj", b"a", "obj");
    let (sha_b, len_b) = put_source(&assets, "b.obj", b"b", "obj");
    let shared = assets.join("shared.meta.ron");
    let meta_b = meta_path_for(&assets.join("b.obj"));
    std::fs::rename(&meta_b, &shared).expect("move b meta");
    std::os::unix::fs::symlink("shared.meta.ron", &meta_b).expect("link");
    let before_a = meta_bytes(&assets, "a.obj");
    let before_shared = std::fs::read(&shared).expect("shared");
    let rows = format!(
        "{}{}",
        mesh_row("a.obj", &sha_a, len_a, ""),
        mesh_row("b.obj", &sha_b, len_b, "")
    );
    let err = import(&assets, &[record("p.toml", "p", &rows)]).expect_err("meta_link");
    assert_eq!(err.code(), CODE_PROVENANCE_IMPORT_CONFLICT, "{err}");
    let ProvenanceImportError::Conflict(report) = err else {
        unreachable!()
    };
    assert!(!report.applied);
    assert_eq!(report.conflicts.len(), 1, "{report:?}");
    assert_eq!(report.conflicts[0].path, "b.obj");
    assert_eq!(report.conflicts[0].kind, "meta_link");
    assert!(CONFLICT_KINDS.contains(&"meta_link"));
    assert!(
        std::fs::symlink_metadata(&meta_b)
            .expect("link")
            .file_type()
            .is_symlink(),
        "the link was replaced"
    );
    assert_eq!(std::fs::read(&shared).expect("shared"), before_shared);
    assert_eq!(meta_bytes(&assets, "a.obj"), before_a, "nothing was written");
}

/// Fix lane: two spellings of one file through a folder link inside `assets/` share one `.meta`:
/// the second is `meta_changed` before anything is written.
#[cfg(unix)]
#[test]
fn two_paths_to_one_sidecar_through_a_folder_link_write_nothing() {
    let dir = TempDir::new("prov-import-folder-alias");
    let assets = dir.join("assets");
    let (sha, len) = put_source(&assets, "x/a.obj", b"aliased", "obj");
    std::os::unix::fs::symlink("x", assets.join("y")).expect("link");
    let before = meta_bytes(&assets, "x/a.obj");
    let rows = format!(
        "{}{}",
        mesh_row("x/a.obj", &sha, len, ""),
        mesh_row("y/a.obj", &sha, len, "")
    );
    let err = import(&assets, &[record("p.toml", "p", &rows)]).expect_err("alias");
    let ProvenanceImportError::Conflict(report) = err else {
        panic!("expected a conflict")
    };
    assert!(!report.applied);
    assert_eq!(report.conflicts.len(), 1, "{report:?}");
    assert_eq!(report.conflicts[0].path, "y/a.obj");
    assert_eq!(report.conflicts[0].kind, "meta_changed");
    assert!(report.conflicts[0].detail.contains("x/a.obj"), "{report:?}");
    assert_eq!(meta_bytes(&assets, "x/a.obj"), before, "nothing was written");
}

/// Fix lane: a source that is a file link leading out of `assets/`, a missing file below a folder
/// link that leads out, and `under` through such a link are all `provenance_import_invalid`.
#[cfg(unix)]
#[test]
fn every_way_out_of_assets_is_refused() {
    let dir = TempDir::new("prov-import-ways-out");
    let assets = dir.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    let outside = dir.join("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(outside.join("s.obj"), b"s").expect("s");
    std::fs::write(
        outside.join("s.obj.meta"),
        MetaSidecar::new("obj", AssetId::new_random())
            .to_ron_string()
            .expect("ron"),
    )
    .expect("outside meta");
    let outside_meta = std::fs::read(outside.join("s.obj.meta")).expect("read");
    let sha = file_sha256_hex(&outside.join("s.obj")).expect("hash");
    std::os::unix::fs::symlink(outside.join("s.obj"), assets.join("file_link.obj")).expect("link");
    std::os::unix::fs::symlink(&outside, assets.join("out")).expect("link");
    for path in ["file_link.obj", "out/missing.obj", "out/s.obj"] {
        let rec = record("p.toml", "p", &mesh_row(path, &sha, 1, ""));
        let err = import(&assets, &[rec]).expect_err(path);
        assert_eq!(err.code(), CODE_PROVENANCE_IMPORT_INVALID, "{path}: {err}");
    }
    let opts = ImportOptions {
        under: Some("out".into()),
        dry_run: false,
    };
    let rec = record("p.toml", "p", &mesh_row("s.obj", &sha, 1, ""));
    let err = import_provenance(&assets, &[rec], &opts).expect_err("under out");
    assert_eq!(err.code(), CODE_PROVENANCE_IMPORT_INVALID, "{err}");
    assert_eq!(
        std::fs::read(outside.join("s.obj.meta")).expect("read"),
        outside_meta
    );
}

/// Fix lane: a missing `assets/` still reports every row as `missing_source` (the under-root check
/// needs a root to resolve, and a missing one holds nothing).
#[test]
fn a_missing_assets_folder_reports_missing_sources() {
    let dir = TempDir::new("prov-import-no-assets");
    let rec = record("p.toml", "p", &mesh_row("a.obj", &"0".repeat(64), 1, ""));
    let report = import(&dir.join("assets"), &[rec]).expect("import");
    assert_eq!(report.missing_source, ["a.obj"]);
    assert!(report.written.is_empty());
}

/// Fix lane: a `.meta` that is a FIFO is `meta_unreadable` (it is never opened, so the call does
/// not block on it).
#[cfg(unix)]
#[test]
fn a_fifo_meta_is_unreadable_and_does_not_block() {
    let dir = TempDir::new("prov-import-fifo-meta");
    let assets = dir.join("assets");
    let (sha, len) = put_source(&assets, "a.obj", b"a", "obj");
    let meta = meta_path_for(&assets.join("a.obj"));
    std::fs::remove_file(&meta).expect("remove meta");
    let made = std::process::Command::new("mkfifo")
        .arg(&meta)
        .status()
        .expect("mkfifo");
    assert!(made.success(), "mkfifo");
    let rec = record("p.toml", "p", &mesh_row("a.obj", &sha, len, ""));
    let err = import(&assets, &[rec]).expect_err("fifo meta");
    let ProvenanceImportError::Conflict(report) = err else {
        panic!("expected a conflict")
    };
    assert_eq!(report.conflicts[0].kind, "meta_unreadable", "{report:?}");
}

/// Fix lane: in a record folder, a link that leaves the folder, a dangling link and a link to a
/// folder are `provenance_import_unreadable`; a folder named `*.toml` is skipped.
#[cfg(unix)]
#[test]
fn record_folder_links_that_leave_or_dangle_are_refused() {
    let dir = TempDir::new("prov-import-folder-links");
    let folder = dir.join("provenance");
    std::fs::create_dir_all(folder.join("sub.toml")).expect("dir");
    std::fs::write(dir.join("elsewhere.toml"), "schema = 3").expect("elsewhere");
    assert_eq!(
        record_files(&folder).expect("list"),
        Vec::<std::path::PathBuf>::new()
    );
    for (name, target) in [
        ("out.toml", dir.join("elsewhere.toml")),
        ("gone.toml", folder.join("nothing-here")),
        ("dir.toml", folder.join("sub.toml")),
    ] {
        let link = folder.join(name);
        std::os::unix::fs::symlink(&target, &link).expect("link");
        let err = record_files(&folder).expect_err(name);
        assert_eq!(err.code(), "provenance_import_unreadable", "{name}: {err}");
        std::fs::remove_file(&link).expect("unlink");
    }
}
