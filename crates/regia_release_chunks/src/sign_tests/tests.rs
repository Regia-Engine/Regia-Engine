// SPDX-License-Identifier: MPL-2.0
//! Acceptance tests for release signing and the filesystem upload backend (ADR-8687).
//!
//! Keys are throwaway Ed25519 text in a temp directory. No test sleeps or reads a clock.
//! The date is an argument, `2026-11-18`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    FsBackend, UploadPlanOptions, VerifyOptions, generate_throwaway_signing_key, publish_gate_ok,
    sign_release, upload_with_backend, verify_release,
};

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "regia-sign-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("temp");
        Self { path }
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn manifest_body() -> &'static [u8] {
    br#"{"format":1,"platform":"demo","version":"1.0.0","chunks":[],"links":[]}"#
}

fn write_manifest(dir: &Path) -> PathBuf {
    let path = dir.join("manifest-demo.json");
    fs::write(&path, manifest_body()).expect("manifest");
    path
}

#[test]
fn tampered_manifest_signature_refused() {
    let dir = Scratch::new("tamper");
    let manifest = write_manifest(&dir);
    let keys = Scratch::new("tamper-key");
    let key = keys.join("release.key");
    fs::write(&key, generate_throwaway_signing_key().expect("throwaway key")).expect("key file");
    sign_release(&dir, &key, "2026-11-18", None).expect("sign");
    let sig = dir.join("manifest-demo.json.sig");
    assert!(
        sig.is_file(),
        "sign writes a detached signature next to the manifest"
    );

    let before = verify_release(&dir, &VerifyOptions::default());
    assert!(
        !before
            .problems
            .iter()
            .any(|problem| problem.check == "manifest_signature"),
        "a fresh signature verifies: {}",
        before
            .problems
            .iter()
            .map(|problem| problem.message.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );

    let mut bytes = fs::read(&manifest).expect("read manifest");
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    fs::write(&manifest, &bytes).expect("tamper");
    let after = verify_release(&dir, &VerifyOptions::default());
    assert!(
        after
            .problems
            .iter()
            .any(|problem| problem.check == "manifest_signature"),
        "a tampered manifest is refused: {}",
        after
            .problems
            .iter()
            .map(|problem| problem.message.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(!after.ok());
}

#[test]
fn a_key_inside_the_project_is_refused() {
    let project = Scratch::new("project");
    fs::write(project.join("Regia.toml"), "name = \"lane\"\n").expect("toml");
    let key = project.join("secret.key");
    let original = b"not a signing key\n";
    fs::write(&key, original).expect("key");
    let dir = Scratch::new("project-release");
    write_manifest(&dir);
    let err = sign_release(&dir, &key, "2026-11-18", Some(&project.path)).expect_err("inside the project");
    assert_eq!(err.code, "content_key_in_project");
    assert_eq!(fs::read(&key).expect("key unchanged"), original);
    assert!(!dir.join("manifest-demo.json.sig").exists());
}

#[test]
fn verify_require_signature_fails_unsigned() {
    let dir = Scratch::new("unsigned");
    write_manifest(&dir);
    let open = verify_release(&dir, &VerifyOptions::default());
    assert!(
        !open
            .problems
            .iter()
            .any(|problem| problem.check == "manifest_signature"),
        "a missing signature is allowed until --require-signature"
    );
    let required = verify_release(
        &dir,
        &VerifyOptions {
            require_signature: true,
            ..VerifyOptions::default()
        },
    );
    assert!(
        required
            .problems
            .iter()
            .any(|problem| problem.check == "manifest_signature"),
        "require-signature refuses an unsigned manifest: {}",
        required
            .problems
            .iter()
            .map(|problem| problem.message.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(!required.ok());
}

#[test]
fn upload_through_the_fs_backend_follows_the_plan() {
    let src = Scratch::new("upload-src");
    fs::write(src.join("core.tar.zst"), b"chunk-bytes").expect("chunk");
    fs::write(src.join("readme.txt"), b"payload").expect("payload");
    fs::write(src.join("manifest-demo.json"), b"{\"format\":1}\n").expect("manifest");
    let dest = Scratch::new("upload-dest");
    let mut backend = FsBackend::create(&dest).expect("dest");
    let receipt = upload_with_backend(&src, &mut backend, &UploadPlanOptions::default()).expect("upload");
    assert_eq!(
        receipt.puts,
        vec![
            "core.tar.zst".to_string(),
            "readme.txt".to_string(),
            "manifest-demo.json".to_string()
        ]
    );
    assert!(receipt.skipped.is_empty());
    assert!(receipt.published);
    assert_eq!(
        fs::read(dest.join("core.tar.zst")).expect("chunk"),
        b"chunk-bytes"
    );
    assert_eq!(fs::read(dest.join("readme.txt")).expect("payload"), b"payload");
    assert_eq!(
        fs::read(dest.join("manifest-demo.json")).expect("manifest"),
        b"{\"format\":1}\n"
    );
    assert!(publish_gate_ok(Some(&dest.join("PUBLISHED"))));

    let again = upload_with_backend(&src, &mut backend, &UploadPlanOptions::default()).expect("again");
    assert!(again.puts.is_empty(), "a matching size is not put again");
    assert_eq!(again.skipped.len(), 3);
    assert!(again.published);
}

#[test]
fn upload_records_plan_order_on_the_fake_and_dry_run_does_not_publish() {
    let src = Scratch::new("fake-src");
    fs::write(src.join("core.tar.zst"), b"chunk-bytes").expect("chunk");
    fs::write(src.join("readme.txt"), b"payload").expect("payload");
    fs::write(src.join("manifest-demo.json"), b"{\"format\":1}\n").expect("manifest");
    let mut fake = super::FakeBackend::default();
    let dry = upload_with_backend(
        &src,
        &mut fake,
        &UploadPlanOptions {
            dry_run: true,
            ..UploadPlanOptions::default()
        },
    )
    .expect("dry");
    assert!(dry.puts.is_empty());
    assert_eq!(dry.skipped.len(), 3);
    assert!(!dry.published);
    assert!(fake.calls.is_empty());

    let receipt = upload_with_backend(&src, &mut fake, &UploadPlanOptions::default()).expect("put");
    assert_eq!(
        receipt.puts,
        vec![
            "core.tar.zst".to_string(),
            "readme.txt".to_string(),
            "manifest-demo.json".to_string()
        ]
    );
    assert_eq!(
        fake.calls,
        vec![
            "put:core.tar.zst".to_string(),
            "put:readme.txt".to_string(),
            "put:manifest-demo.json".to_string(),
            "publish".to_string(),
        ]
    );
}

#[test]
fn a_bad_date_is_release_date_and_a_flipped_signature_byte_is_refused() {
    let dir = Scratch::new("date");
    write_manifest(&dir);
    let keys = Scratch::new("date-key");
    let key = keys.join("release.key");
    fs::write(&key, generate_throwaway_signing_key().expect("key")).expect("key file");
    let err = sign_release(&dir, &key, "2026-02-31", None).expect_err("bad date");
    assert_eq!(err.code, "release_date");
    assert!(!dir.join("manifest-demo.json.sig").exists());

    sign_release(&dir, &key, "2026-11-18", None).expect("sign");
    let sig_path = dir.join("manifest-demo.json.sig");
    let text = fs::read_to_string(&sig_path).expect("sig");
    let marker = "\"signature\": \"";
    let Some(index) = text.find(marker) else {
        panic!("signature field: {text}");
    };
    let hex_at = index + marker.len();
    let mut bytes = text.into_bytes();
    bytes[hex_at] = if bytes[hex_at] == b'a' { b'b' } else { b'a' };
    fs::write(&sig_path, bytes).expect("rewrite");
    let report = verify_release(&dir, &VerifyOptions::default());
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.check == "manifest_signature")
    );
    assert!(!report.ok());
}

/// The registry example (`docs/formats/release-signature.example.json`, also the corpus fixture
/// `format-corpus/release-signature/v1.json`) is byte for byte what the writer signs today for
/// the seed `[7; 32]`, and the canonical reader writes it back unchanged (ADR-8846).
#[test]
fn the_signature_example_is_what_the_writer_writes() {
    let example = include_bytes!("../../../../docs/formats/release-signature.example.json");
    let written =
        super::sign::signature_for_seed(&[7; 32], "manifest-demo.json", manifest_body(), "2026-11-18");
    assert_eq!(
        String::from_utf8_lossy(&written),
        String::from_utf8_lossy(example),
        "the example must be re-made when the writer changes (and the format bumped)"
    );
    assert_eq!(
        super::canonical_signature_json(example).expect("canonical"),
        example.to_vec()
    );
    assert_eq!(super::peek_signature_version(example).expect("version"), 1);

    let dir = Scratch::new("example");
    write_manifest(&dir);
    fs::write(dir.join("manifest-demo.json.sig"), example).expect("sig");
    let report = verify_release(&dir, &VerifyOptions::default());
    assert!(
        !report
            .problems
            .iter()
            .any(|problem| problem.check == "manifest_signature"),
        "the example verifies against its manifest: {:?}",
        report.problems
    );
}

/// Shape checks of the canonical reader: each is refused with a stable code, and the field
/// list is the frozen fingerprint (ADR-8846).
#[test]
fn the_signature_reader_refuses_bad_shapes() {
    let example = std::str::from_utf8(include_bytes!(
        "../../../../docs/formats/release-signature.example.json"
    ))
    .expect("utf-8")
    .to_string();
    let code = |text: &str| {
        super::canonical_signature_json(text.as_bytes())
            .map(|_| ())
            .map_err(|err| err.code)
    };
    assert_eq!(
        code(&example.replace("\"format\": 1", "\"format\": 2")),
        Err("release_signature_newer")
    );
    assert_eq!(
        code(&example.replace("\"format\": 1,", "\"format\": 1,\n  \"comment\": \"x\",")),
        Err("manifest_signature"),
        "unknown fields are refused"
    );
    assert_eq!(
        code(&example.replace("ed25519\"", "rsa\"")),
        Err("manifest_signature")
    );
    assert_eq!(
        code(&example.replace("2026-11-18", "2026-02-31")),
        Err("manifest_signature")
    );
    assert_eq!(
        code(&example.replace("0871f3aabc26e458", "0000000000000000")),
        Err("manifest_signature")
    );
    assert_eq!(
        code(&example.replace("\"signature\": \"80", "\"signature\": \"")),
        Err("manifest_signature")
    );
    assert_eq!(
        code(&example.replace("\"manifest_sha256\": \"ea", "\"manifest_sha256\": \"EA")),
        Err("manifest_signature")
    );
    assert_eq!(code("[]"), Err("manifest_signature"));
    assert_eq!(code(&" ".repeat(9000)), Err("manifest_signature"));
    assert_eq!(
        super::signature_schema_fingerprint(),
        "format,algorithm,key_id,public_key,date,manifest_sha256,signature"
    );
}
