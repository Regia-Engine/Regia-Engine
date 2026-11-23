// SPDX-License-Identifier: MPL-2.0
//! Acceptance and format tests. Temps use the process id plus a counter.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use super::tar::MemberKind;
use super::{
    AuditSettings, ChunkCodec, ChunkError, ChunkLink, ChunkSettings, CutRequest, DEFAULT_MAX_CHUNK_BYTES,
    FORMAT_VERSION, Identity, PackRequest, TWO_GIB, ZstdCli, bucket_index, content_hash, cut_tree, fnv1a64,
    load_rules_str, mesh_id, migrate_json, pack_release, parse_byte_size, peek_version, plan_tree,
    schema_fingerprint, sha1, sha256_hex, verify_tree,
};

use super::manifest::{CANONICAL_FILE, parse_manifest};

#[test]
fn chunk_never_exceeds_cap() {
    let scratch = Scratch::new("cap");
    let src = scratch.join("src");
    let out = scratch.join("out");
    for name in ["a.bin", "b.bin", "c.bin"] {
        write_bytes(&src.join(name), &vec![b'x'; 300]);
    }
    let mut settings = wide();
    settings.max_chunk_bytes = 2500;
    let manifest = cut_tree(&cut_req(&src, &out, settings), &Identity).expect("chunks stay under the cap");
    assert!(
        manifest.chunks.len() >= 2,
        "three files need more than one chunk, got {}",
        manifest.chunks.len()
    );
    for chunk in &manifest.chunks {
        assert!(chunk.size <= 2500, "{} is {} bytes", chunk.name, chunk.size);
        let on_disk = std::fs::metadata(out.join(&chunk.asset))
            .expect("chunk file")
            .len();
        assert_eq!(on_disk, chunk.size);
    }
    assert!(leftover(&out).iter().all(|name| !name.starts_with('.')));
}

#[test]
fn one_file_identity_tar_is_header_block_and_trailer() {
    let scratch = Scratch::new("one");
    let src = scratch.join("src");
    let out = scratch.join("out");
    write_bytes(&src.join("a.bin"), &vec![b'x'; 300]);
    let manifest = cut_tree(&cut_req(&src, &out, wide()), &Identity).expect("cut");
    assert_eq!(manifest.chunks.len(), 1);
    assert_eq!(manifest.chunks[0].size, 2048);
    assert_eq!(manifest.chunks[0].unpacked, 300);
}

#[test]
fn plan_counts_a_split_when_one_archive_would_pass_the_cap() {
    let scratch = Scratch::new("plan");
    let src = scratch.join("src");
    for name in ["a.bin", "b.bin", "c.bin"] {
        write_bytes(&src.join(name), &vec![b'x'; 300]);
    }
    let mut settings = wide();
    settings.max_chunk_bytes = 2500;
    let plan = plan_tree(&src, &settings).expect("plan");
    let core = plan
        .groups
        .iter()
        .find(|group| group.group == "core")
        .expect("core");
    assert!(core.estimated_chunks >= 2);
    assert_eq!(core.files, 3);
    assert_eq!(core.raw_bytes, 900);
}

#[test]
fn oversize_file_leaves_no_chunk() {
    let scratch = Scratch::new("over");
    let src = scratch.join("src");
    let out = scratch.join("out");
    write_bytes(&src.join("big.bin"), &vec![b'z'; 300]);
    let mut settings = wide();
    settings.max_chunk_bytes = 1000;
    let err = cut_tree(&cut_req(&src, &out, settings), &Identity).expect_err("one file cannot be split");
    assert_eq!(err.code, "chunk_over_cap");
    assert!(leftover(&out).is_empty(), "{:?}", leftover(&out));
}

#[test]
fn cap_at_two_gib_is_rejected_and_the_default_is_under_it() {
    const {
        assert!(DEFAULT_MAX_CHUNK_BYTES < TWO_GIB);
    };
    assert_eq!(parse_byte_size("2GiB").expect("2GiB"), TWO_GIB);
    assert_eq!(parse_byte_size("1MiB").expect("1MiB"), 1024 * 1024);
    assert_eq!(parse_byte_size("2KiB").expect("2KiB"), 2048);
    assert_eq!(parse_byte_size("10").expect("10"), 10);
    assert_eq!(
        parse_byte_size("nope").expect_err("text").code,
        "chunk_cap_invalid"
    );
    let scratch = Scratch::new("capinv");
    let src = scratch.join("src");
    let out = scratch.join("out");
    write_bytes(&src.join("a.bin"), b"a");
    let mut settings = wide();
    settings.max_chunk_bytes = TWO_GIB;
    assert_eq!(
        cut_tree(&cut_req(&src, &out, settings), &Identity)
            .expect_err("cap")
            .code,
        "chunk_cap_invalid"
    );
    let mut zero = wide();
    zero.max_chunk_bytes = 0;
    assert_eq!(
        cut_tree(&cut_req(&src, &out, zero), &Identity)
            .expect_err("zero")
            .code,
        "chunk_cap_invalid"
    );
    assert!(std::fs::metadata(&out).is_err());
}

#[test]
fn two_cuts_match_byte_for_byte_and_the_sidecars_agree() {
    let scratch = Scratch::new("det");
    let src = scratch.join("src");
    write_bytes(&src.join("note.txt"), b"same bytes");
    let mut first_req = cut_req(&src, &scratch.join("out1"), wide());
    first_req.platform = "linux".to_string();
    first_req.tag = "demo".to_string();
    first_req.product_version = "0.6.0".to_string();
    first_req.source_commit = "d6ed37677".to_string();
    let mut second_req = first_req.clone();
    second_req.output = scratch.join("out2");
    let first = cut_tree(&first_req, &Identity).expect("first");
    let second = cut_tree(&second_req, &Identity).expect("second");
    assert_eq!(
        first.to_json_bytes().expect("json"),
        second.to_json_bytes().expect("json")
    );
    let left = std::fs::read(first_req.output.join(&first.chunks[0].asset)).expect("left");
    let right = std::fs::read(second_req.output.join(&second.chunks[0].asset)).expect("right");
    assert_eq!(left, right);
    let json = std::fs::read(first_req.output.join(CANONICAL_FILE)).expect("manifest");
    assert_eq!(parse_manifest(&json).expect("parse"), first);
    let tsv = std::fs::read_to_string(first_req.output.join("release-chunks.tsv")).expect("tsv");
    assert!(tsv.contains(&first.chunks[0].sha256));
    assert!(tsv.starts_with("format\t1\n"));
    let json_sha = sha256_hex(&json);
    let tsv_sha = sha256_hex(tsv.as_bytes());
    let sidecar = std::fs::read_to_string(first_req.output.join("release-chunks.json.sha256")).expect("sha");
    assert_eq!(
        sidecar,
        format!("{json_sha}  {CANONICAL_FILE}\n{tsv_sha}  release-chunks.tsv\n")
    );
    let copy = std::fs::read(first_req.output.join("manifest-linux.json")).expect("platform copy");
    assert_eq!(copy, json);
    verify_tree(&first_req.output, wide().max_chunk_bytes).expect("verify");
}

#[test]
fn verify_rejects_a_flipped_byte() {
    let scratch = Scratch::new("flip");
    let src = scratch.join("src");
    let out = scratch.join("out");
    write_bytes(&src.join("a.bin"), &vec![b'x'; 300]);
    let manifest = cut_tree(&cut_req(&src, &out, wide()), &Identity).expect("cut");
    let path = out.join(&manifest.chunks[0].asset);
    let mut bytes = std::fs::read(&path).expect("read");
    let index = bytes.len() - 1;
    bytes[index] ^= 0xff;
    std::fs::write(&path, bytes).expect("write");
    let err = verify_tree(&out, wide().max_chunk_bytes).expect_err("flipped");
    assert_eq!(err.code, "sha256_mismatch");
}

#[test]
fn release_layout_groups_files() {
    let scratch = Scratch::new("layout");
    let src = scratch.join("src");
    write_bytes(&src.join("readme.txt"), b"root");
    write_bytes(&src.join("config/app.txt"), b"cfg");
    write_bytes(&src.join("cooked/tex/a.bin"), b"tex");
    write_bytes(&src.join("cooked/mesh/a.bin"), b"mesh");
    write_bytes(&src.join("models/hero/a.bin"), b"hero");
    write_bytes(&src.join("other/a.bin"), b"other");
    let mut settings = wide();
    settings.split_roots = vec!["models".to_string()];
    let manifest = cut_tree(&cut_req(&src, &scratch.join("out"), settings), &Identity).expect("cut");
    let mut groups: Vec<&str> = manifest.chunks.iter().map(|chunk| chunk.group.as_str()).collect();
    groups.sort_unstable();
    assert_eq!(
        groups,
        vec!["cooked", "cooked-tex", "core", "dir-other", "models-hero"]
    );
    let core = manifest
        .chunks
        .iter()
        .find(|chunk| chunk.group == "core")
        .expect("core");
    assert_eq!(
        core.files,
        vec!["config/app.txt".to_string(), "readme.txt".to_string()]
    );
}

#[test]
fn rules_use_the_first_match() {
    let ordered =
        load_rules_str("[[rule]]\ngroup = \"first\"\nglobs = [\"*.txt\"]\n\nsecond = [\"*.txt\"]\n")
            .expect("rules");
    assert_eq!(ordered.assign("dir/a.txt"), "first");
    assert_eq!(ordered.assign("a.bin"), "default");
    let cooked = load_rules_str("[cooked]\nglobs = [\"cooked/**\"]\n").expect("table");
    assert_eq!(cooked.assign("cooked/tex/a.bin"), "cooked");
    assert_eq!(
        load_rules_str("nope = 1").expect_err("shape").code,
        "rules_invalid"
    );
    let scratch = Scratch::new("rules");
    let src = scratch.join("src");
    write_bytes(&src.join("notes/a.txt"), b"n");
    write_bytes(&src.join("other.bin"), b"o");
    let mut settings = wide();
    settings.rules = Some(ordered);
    let manifest = cut_tree(&cut_req(&src, &scratch.join("out"), settings), &Identity).expect("cut");
    let mut groups: Vec<&str> = manifest.chunks.iter().map(|chunk| chunk.group.as_str()).collect();
    groups.sort_unstable();
    assert_eq!(groups, vec!["default", "first"]);
}

#[test]
fn known_hash_vectors_and_the_schema_key_order() {
    assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(content_hash(b""), "cbf29ce484222325");
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
    assert_eq!(bucket_index("abc", 2).expect("buckets"), 0xa999_3e36_u32 % 2);
    let id = mesh_id(b"xyz", &[1, 2]).expect("mesh");
    assert_eq!(id.len(), 24);
    let mut geom = b"xyz".to_vec();
    geom.extend(1u32.to_le_bytes());
    geom.extend(2u32.to_le_bytes());
    assert_eq!(&id[..16], content_hash(&geom));
    assert!(id.ends_with("00000003"));
    let example = super::canonical_example();
    let text = String::from_utf8(example.to_json_bytes().expect("json")).expect("utf8");
    assert_key_order(
        &text,
        &[
            "format_version",
            "platform",
            "tag",
            "product_version",
            "source_commit",
            "libc_required",
            "tier",
            "chunks",
            "links",
        ],
    );
    let chunk_at = text.find("\"chunks\"").expect("chunks");
    assert_key_order(
        &text[chunk_at..],
        &[
            "name",
            "group",
            "buckets",
            "asset",
            "sha256",
            "size",
            "unpacked",
            "files",
            "content_key",
        ],
    );
    let link = serde_json::to_string(&ChunkLink {
        path: "a".to_string(),
        target: "b".to_string(),
        chunk: "c".to_string(),
    })
    .expect("link");
    assert_key_order(&link, &["path", "target", "chunk"]);
    assert_eq!(
        schema_fingerprint(),
        "format_version,platform,tag,product_version,source_commit,libc_required,tier,chunks,links;name,group,buckets,asset,sha256,size,unpacked,files,content_key;path,target,chunk"
    );
    assert!(text.contains("d6ed37677"));
    let mut value = serde_json::to_value(&example).expect("value");
    value
        .as_object_mut()
        .expect("obj")
        .insert("extra".to_string(), serde_json::json!(1));
    let extra = serde_json::to_vec(&value).expect("extra");
    assert_eq!(
        parse_manifest(&extra).expect_err("unknown").code,
        "manifest_invalid"
    );
    let mut newer = example;
    newer.format_version = 2;
    let bytes = serde_json::to_vec(&newer).expect("v2");
    assert_eq!(peek_version(&bytes).expect("peek"), 2);
    let err = migrate_json(&bytes).expect_err("newer");
    assert_eq!(err.code, "release_chunks_newer");
    assert!(err.message.contains('2'));
    let current = super::canonical_example().to_json_bytes().expect("v1");
    assert_eq!(migrate_json(&current).expect("migrate"), current);
    assert_eq!(FORMAT_VERSION, 1);
}

#[test]
fn directory_fingerprint_skips_bins_that_miss_the_magic() {
    let scratch = Scratch::new("audit");
    let src = scratch.join("src");
    write_bytes(&src.join("mesh/hero.gltf"), b"gltf-bytes");
    write_bytes(&src.join("a.bin"), &[1, 2, 3, 4, 5]);
    write_bytes(&src.join("b.bin"), &[9, 9, 9, 9]);
    let prints = super::audit_tree(&src, &AuditSettings::default()).expect("audit");
    let mesh = prints.iter().find(|print| print.path == "mesh").expect("mesh");
    assert_eq!(mesh.hash.len(), 16);
    let mut buf = Vec::new();
    buf.extend(b"hero.gltf");
    buf.push(0);
    buf.extend(&10u64.to_le_bytes());
    assert_eq!(mesh.hash, content_hash(&buf));
    let magic = AuditSettings {
        magic: Some(vec![1, 2, 3, 4]),
        ..Default::default()
    };
    let filtered = super::audit_tree(&src, &magic).expect("magic");
    let root = filtered.iter().find(|print| print.path.is_empty()).expect("root");
    let mut only = Vec::new();
    only.extend(b"a.bin");
    only.push(0);
    only.extend(&5u64.to_le_bytes());
    assert_eq!(root.hash, content_hash(&only));
    let dest = scratch.join("dir_prints.txt");
    super::write_dir_prints(&src, &AuditSettings::default(), &dest).expect("write");
    let text = std::fs::read_to_string(&dest).expect("text");
    assert!(text.contains(&mesh.hash));
}

#[test]
fn zstd_round_trip_when_the_program_exists() {
    assert_eq!(
        ZstdCli::new(0, 1, false).expect_err("level").code,
        "zstd_level_invalid"
    );
    assert_eq!(
        ZstdCli::new(3, 0, false).expect_err("threads").code,
        "zstd_level_invalid"
    );
    let present = std::process::Command::new("zstd")
        .arg("-V")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    let scratch = Scratch::new("zstd");
    let src = scratch.join("src");
    let out = scratch.join("out");
    write_bytes(&src.join("note.txt"), b"hello release");
    match cut_tree(&cut_req(&src, &out, wide()), &ZstdCli::standard()) {
        Ok(manifest) => {
            assert!(present, "zstd ran but -V was not successful");
            assert!(manifest.chunks[0].asset.ends_with(".tar.zst"));
            assert_ne!(manifest.chunks[0].sha256, manifest.chunks[0].content_key);
            verify_tree(&out, wide().max_chunk_bytes).expect("verify");
        }
        Err(err) if err.code == "zstd_missing" && !present => {}
        Err(err) => panic!("unexpected zstd failure: {err}"),
    }
}

#[test]
fn pack_reuses_bytes_keeps_bucket_counts_and_limits_tier_diffs() {
    let scratch = Scratch::new("pack");
    let src = scratch.join("src");
    write_bytes(&src.join("data/a.bin"), &[1u8; 40]);
    write_bytes(&src.join("data/b.bin"), &[2u8; 40]);
    write_bytes(&src.join("data/c.bin"), &[3u8; 40]);
    let mut settings = wide();
    settings.bucket_raw = 50;
    let out1 = scratch.join("out1");
    let first = pack_release(&pack_req(&src, &out1, settings.clone()), &Stamp(1)).expect("pack");
    assert!(first.chunks.iter().all(|chunk| chunk.buckets > 1));
    let floor = first.chunks[0].buckets;
    let mut wide_buckets = wide();
    wide_buckets.bucket_raw = DEFAULT_MAX_CHUNK_BYTES;
    let second = pack_release(
        &pack_req_prev(&src, &scratch.join("out2"), wide_buckets, &out1),
        &Stamp(2),
    )
    .expect("reuse");
    assert!(second.chunks.iter().all(|chunk| chunk.buckets == floor));
    let left = std::fs::read(out1.join(&first.chunks[0].asset)).expect("old");
    let reused = second
        .chunks
        .iter()
        .find(|chunk| chunk.content_key == first.chunks[0].content_key)
        .expect("same content");
    let right = std::fs::read(scratch.join("out2").join(&reused.asset)).expect("new");
    assert_eq!(
        left, right,
        "reuse must copy the previous chunk instead of recompressing"
    );
    assert_eq!(*left.last().expect("stamp"), 1);

    std::fs::remove_file(out1.join(&first.chunks[0].asset)).expect("delete");
    let err = pack_release(
        &pack_req_prev(&src, &scratch.join("out3"), settings, &out1),
        &Stamp(1),
    )
    .expect_err("missing asset");
    assert_eq!(err.code, "reuse_mismatch");
}

#[test]
fn tier_may_replace_only_the_texture_group() {
    let scratch = Scratch::new("tier");
    let src1 = scratch.join("src1");
    let src2 = scratch.join("src2");
    let src3 = scratch.join("src3");
    write_bytes(&src1.join("config/app.txt"), b"same");
    write_bytes(&src1.join("cooked/tex/a.bin"), b"tex-v1");
    write_bytes(&src2.join("config/app.txt"), b"same");
    write_bytes(&src2.join("cooked/tex/a.bin"), b"tex-v2");
    write_bytes(&src3.join("config/app.txt"), b"changed");
    write_bytes(&src3.join("cooked/tex/a.bin"), b"tex-v2");
    let out1 = scratch.join("out1");
    pack_release(&pack_req(&src1, &out1, wide()), &Identity).expect("base");
    pack_release(
        &pack_req_prev(&src2, &scratch.join("out2"), wide(), &out1),
        &Identity,
    )
    .expect("tier");
    let err = pack_release(
        &pack_req_prev(&src3, &scratch.join("out3"), wide(), &out1),
        &Identity,
    )
    .expect_err("core changed");
    assert_eq!(err.code, "tier_chunk_differs");
}

#[test]
fn pack_refuses_a_chunk_over_the_cap() {
    let scratch = Scratch::new("packcap");
    let src = scratch.join("src");
    let out = scratch.join("out");
    write_bytes(&src.join("data/big.bin"), &vec![b'q'; 300]);
    let mut settings = wide();
    settings.max_chunk_bytes = 1000;
    let err = pack_release(&pack_req(&src, &out, settings), &Identity).expect_err("cap");
    assert_eq!(err.code, "chunk_over_cap");
    assert!(leftover(&out).is_empty(), "{:?}", leftover(&out));
}

#[test]
fn a_tab_in_a_path_is_rejected() {
    let err = super::scan::check_rel("a\tb").expect_err("tab");
    assert_eq!(err.code, "path_rejected");
    let err = super::scan::check_rel(&"a".repeat(4097)).expect_err("long");
    assert_eq!(err.code, "path_too_long");
}

#[test]
fn a_long_member_name_round_trips_through_pax() {
    let scratch = Scratch::new("pax");
    let src = scratch.join("src");
    let out = scratch.join("out");
    let name = "n".repeat(180);
    write_bytes(&src.join(&name), b"pax");
    let manifest = cut_tree(&cut_req(&src, &out, wide()), &Identity).expect("cut");
    assert_eq!(manifest.chunks[0].files, vec![name]);
    verify_tree(&out, wide().max_chunk_bytes).expect("verify");
}

#[cfg(unix)]
#[test]
fn unix_hard_link_is_stored_once() {
    let scratch = Scratch::new("hard");
    let src = scratch.join("src");
    let out = scratch.join("out");
    write_bytes(&src.join("b.bin"), &vec![7u8; 300]);
    std::fs::hard_link(src.join("b.bin"), src.join("a.bin")).expect("link");
    let manifest = cut_tree(&cut_req(&src, &out, wide()), &Identity).expect("cut");
    assert_eq!(manifest.chunks[0].unpacked, 300);
    assert_eq!(
        manifest.chunks[0].files,
        vec!["a.bin".to_string(), "b.bin".to_string()]
    );
    assert_eq!(manifest.links.len(), 1);
    assert_eq!(manifest.links[0].path, "b.bin");
    assert_eq!(manifest.links[0].target, "a.bin");
    let members = super::tar::read_members(&out.join(&manifest.chunks[0].asset)).expect("tar");
    assert_eq!(members.len(), 2);
    assert_eq!(members[0].kind, MemberKind::File);
    assert_eq!(members[0].size, 300);
    assert_eq!(members[1].kind, MemberKind::Hardlink);
    assert_eq!(members[1].link.as_deref(), Some("a.bin"));
    verify_tree(&out, wide().max_chunk_bytes).expect("verify");
}

#[cfg(unix)]
#[test]
fn unix_symlink_and_socket_follow_cut_and_pack_rules() {
    let scratch = Scratch::new("sym");
    let src = scratch.join("src");
    let out = scratch.join("out");
    write_bytes(&src.join("a.bin"), b"file");
    std::os::unix::fs::symlink("a.bin", src.join("link.bin")).expect("symlink");
    let manifest = cut_tree(&cut_req(&src, &out, wide()), &Identity).expect("cut");
    assert!(manifest.chunks[0].files.iter().any(|file| file == "link.bin"));
    verify_tree(&out, wide().max_chunk_bytes).expect("verify");

    let bad = scratch.join("bad");
    std::fs::create_dir_all(&bad).expect("bad");
    std::os::unix::fs::symlink("/etc/passwd", bad.join("abs")).expect("abs");
    let err = cut_tree(&cut_req(&bad, &scratch.join("bad-out"), wide()), &Identity).expect_err("absolute");
    assert_eq!(err.code, "path_rejected");

    let pack_src = scratch.join("pack-src");
    write_bytes(&pack_src.join("a.bin"), b"file");
    std::os::unix::fs::symlink("a.bin", pack_src.join("link.bin")).expect("pack link");
    let err =
        pack_release(&pack_req(&pack_src, &scratch.join("pack-out"), wide()), &Identity).expect_err("link");
    assert_eq!(err.code, "not_regular");

    let sock_src = scratch.join("sock-src");
    std::fs::create_dir_all(&sock_src).expect("sock dir");
    write_bytes(&sock_src.join("a.bin"), b"file");
    let listener = std::os::unix::net::UnixListener::bind(sock_src.join("sock")).expect("socket");
    let err = pack_release(
        &pack_req(&sock_src, &scratch.join("sock-pack"), wide()),
        &Identity,
    )
    .expect_err("socket");
    assert_eq!(err.code, "not_regular");
    let cut_manifest =
        cut_tree(&cut_req(&sock_src, &scratch.join("sock-cut"), wide()), &Identity).expect("skip socket");
    assert_eq!(cut_manifest.chunks[0].files, vec!["a.bin".to_string()]);
    drop(listener);
}

struct Stamp(u8);

impl ChunkCodec for Stamp {
    fn name(&self) -> &'static str {
        "stamp"
    }

    fn extension(&self) -> &'static str {
        "bin"
    }

    fn compress_file(&self, input: &Path, output: &Path) -> Result<(), ChunkError> {
        let mut bytes = std::fs::read(input).map_err(|err| ChunkError::io(input, &err))?;
        bytes.push(self.0);
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).map_err(|err| ChunkError::io(parent, &err))?;
        }
        std::fs::write(output, bytes).map_err(|err| ChunkError::io(output, &err))
    }

    fn decompress_file(&self, input: &Path, output: &Path) -> Result<(), ChunkError> {
        let mut bytes = std::fs::read(input).map_err(|err| ChunkError::io(input, &err))?;
        if bytes.pop() != Some(self.0) {
            return Err(ChunkError::new("verify_failed", "stamp byte is missing"));
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).map_err(|err| ChunkError::io(parent, &err))?;
        }
        std::fs::write(output, bytes).map_err(|err| ChunkError::io(output, &err))
    }
}

fn wide() -> ChunkSettings {
    ChunkSettings {
        max_chunk_bytes: 8 * 1024 * 1024,
        ..Default::default()
    }
}

fn cut_req(src: &Path, out: &Path, settings: ChunkSettings) -> CutRequest {
    CutRequest {
        source: src.to_path_buf(),
        output: out.to_path_buf(),
        settings,
        platform: String::new(),
        tag: String::new(),
        product_version: String::new(),
        source_commit: String::new(),
        libc_required: String::new(),
        tier: String::new(),
    }
}

fn pack_req(src: &Path, out: &Path, settings: ChunkSettings) -> PackRequest {
    PackRequest {
        source: src.to_path_buf(),
        output: out.to_path_buf(),
        settings,
        platform: String::new(),
        tag: String::new(),
        product_version: String::new(),
        source_commit: String::new(),
        libc_required: String::new(),
        tier: String::new(),
        previous_dir: None,
    }
}

fn pack_req_prev(src: &Path, out: &Path, settings: ChunkSettings, previous: &Path) -> PackRequest {
    let mut request = pack_req(src, out, settings);
    request.previous_dir = Some(previous.to_path_buf());
    request
}

fn write_bytes(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent");
    }
    std::fs::write(path, bytes).expect("write");
}

fn leftover(out: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir(out) else {
        return names;
    };
    for entry in entries {
        let entry = entry.expect("dir entry");
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    names
}

fn assert_key_order(json: &str, keys: &[&str]) {
    let mut last = 0usize;
    for key in keys {
        let needle = format!("\"{key}\"");
        let at = json.find(&needle).unwrap_or_else(|| panic!("missing {key}"));
        assert!(at >= last, "{key} is out of order in {json}");
        last = at + needle.len();
    }
}

fn hex(bytes: &[u8]) -> String {
    super::hash::hex_encode(bytes)
}

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "regia-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("temp");
        Self { path }
    }

    fn join(&self, rel: &str) -> PathBuf {
        self.path.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn canonical_example_matches_the_corpus_fixture() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../regia_agent/tests/fixtures/format-corpus/release-chunks/v1.json");
    let fixture = std::fs::read(&path).expect("corpus fixture");
    let written = super::canonical_example().to_json_bytes().expect("json");
    assert_eq!(
        fixture, written,
        "the corpus fixture must be to_json_bytes of canonical_example"
    );
}

/// A reversible run-length codec for tests: `(count, byte)` pairs. Tar padding and repeated
/// bytes shrink a lot, so a compressed plan can differ from the raw estimate.
struct RunLength;

impl ChunkCodec for RunLength {
    fn name(&self) -> &'static str {
        "run-length"
    }

    fn extension(&self) -> &'static str {
        "tar.rl"
    }

    fn compress_file(&self, input: &Path, output: &Path) -> Result<(), ChunkError> {
        let bytes = std::fs::read(input).map_err(|err| ChunkError::io(input, &err))?;
        let mut out = Vec::new();
        let mut rest = bytes.as_slice();
        while let Some(&first) = rest.first() {
            let run = rest.iter().take(255).take_while(|&&b| b == first).count();
            out.push(u8::try_from(run).expect("run fits a byte"));
            out.push(first);
            rest = &rest[run..];
        }
        std::fs::write(output, out).map_err(|err| ChunkError::io(output, &err))
    }

    fn decompress_file(&self, input: &Path, output: &Path) -> Result<(), ChunkError> {
        let bytes = std::fs::read(input).map_err(|err| ChunkError::io(input, &err))?;
        let (pairs, _) = bytes.as_chunks::<2>();
        let out: Vec<u8> = pairs
            .iter()
            .flat_map(|[count, byte]| std::iter::repeat_n(*byte, usize::from(*count)))
            .collect();
        std::fs::write(output, out).map_err(|err| ChunkError::io(output, &err))
    }
}

/// C-09 compressed-size planning (ADR-8846): the raw estimate counts a split the compressed
/// cut never makes; `plan_tree_compressed` counts what `cut` writes with the same codec, with
/// its compressed bytes, and leaves nothing in the scratch directory.
#[test]
fn compressed_plan_counts_what_cut_writes() {
    let scratch = Scratch::new("plan-compressed");
    let src = scratch.join("src");
    let out = scratch.join("out");
    let work = scratch.join("work");
    std::fs::create_dir_all(&work).expect("work");
    for name in ["a.bin", "b.bin", "c.bin"] {
        write_bytes(&src.join(name), &vec![b'x'; 300]);
    }
    let mut settings = wide();
    settings.max_chunk_bytes = 2500;
    let raw = plan_tree(&src, &settings).expect("raw plan");
    let core = |plan: &crate::ReleasePlan| {
        plan.groups
            .iter()
            .find(|g| g.group == "core")
            .cloned()
            .expect("core")
    };
    assert!(
        core(&raw).estimated_chunks >= 2,
        "the raw tar of three files passes 2500 bytes"
    );
    assert_eq!(core(&raw).compressed_bytes, None);

    let planned = crate::plan_tree_compressed(&src, &settings, &RunLength, &work).expect("compressed plan");
    let manifest = cut_tree(&cut_req(&src, &out, settings.clone()), &RunLength).expect("cut");
    let written: Vec<_> = manifest.chunks.iter().filter(|c| c.group == "core").collect();
    assert_eq!(written.len(), 1, "run-length packs all three into one chunk");
    assert_eq!(core(&planned).estimated_chunks, 1);
    assert_eq!(core(&planned).compressed_bytes, Some(written[0].size));
    assert_eq!(core(&planned).files, 3);
    assert_eq!(core(&planned).raw_bytes, 900);
    assert!(
        leftover(&work).is_empty(),
        "scratch left behind: {:?}",
        leftover(&work)
    );

    let identity = crate::plan_tree_compressed(&src, &settings, &Identity, &work).expect("identity plan");
    let id_out = scratch.join("id-out");
    let id_manifest = cut_tree(&cut_req(&src, &id_out, settings.clone()), &Identity).expect("identity cut");
    let id_written = id_manifest.chunks.iter().filter(|c| c.group == "core").count();
    assert_eq!(core(&identity).estimated_chunks as usize, id_written);

    write_bytes(&src.join("big.bin"), &vec![b'z'; 3000]);
    let err =
        crate::plan_tree_compressed(&src, &settings, &Identity, &work).expect_err("one file over the cap");
    assert_eq!(err.code, "chunk_over_cap");
    assert!(
        leftover(&work).is_empty(),
        "a failed plan leaves nothing: {:?}",
        leftover(&work)
    );
}
