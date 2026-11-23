// SPDX-License-Identifier: MPL-2.0
//! Detached Ed25519 signatures for a release manifest (ADR-8687).
//!
//! Requirements at pin `c9513623f` (`regia-absorb-4`): dated evidence, a SHA-256 of the
//! manifest, signed, with the date. The shell script was not copied. There is no git tag,
//! no timestamp calendar, and no network. The private key is the file `regia assets keygen`
//! writes. The signed payload is the manifest file name, its SHA-256, and the date. A
//! detached `.sig` sits next to the manifest. The domain prefix is release-specific, so a
//! content-manifest signature cannot be replayed here.
//!
//! A key path inside `project` is `content_key_in_project` and the key file is not read.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ChunkError;
use crate::hash::{hex_encode, sha256_file, sha256_hex};
use crate::verify::{VerifyOptions, VerifyProblem, list_manifests};

/// Domain prefix for the signed message. The payload bytes follow it.
const RELEASE_SIGNATURE_DOMAIN: &[u8] = b"regia-release-manifest-v1\0";

/// First line of a private key file written by `regia assets keygen`.
const SIGNING_KEY_HEADER: &str = "regia-content-signing-key v1";

/// `format` field of a release `.sig` file.
pub const RELEASE_SIGNATURE_FORMAT: u32 = 1;

/// Registry name of the `.sig` sidecar in `docs/formats/format-hashes.ron` (ADR-8846).
pub const RELEASE_SIGNATURE_FORMAT_NAME: &str = "release-signature";

/// Suffix of a sidecar next to a release manifest (`manifest-<platform>.json.sig`).
pub const RELEASE_SIGNATURE_SUFFIX: &str = ".json.sig";

/// Largest `.sig` file read.
const MAX_SIGNATURE_BYTES: u64 = 8192;

/// Files [`sign_release`] signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignReport {
    /// Manifest file names that received a `.sig`.
    pub files: Vec<String>,
}

/// One manifest in [`SignStatusReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestSignStatus {
    /// Manifest file name.
    pub file: String,
    /// A `.sig` file sits next to the manifest.
    pub signed: bool,
    /// The signature matches the current bytes, the date, and the key.
    pub verified: bool,
    /// Date from the signature, or empty when there is none.
    pub date: String,
    /// Key id from the signature, or empty when there is none.
    pub key_id: String,
    /// SHA-256 of the manifest bytes on disk, or empty when the file cannot be read.
    pub manifest_sha256: String,
}

/// Result of [`sign_status`]. A missing directory is a report, not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignStatusReport {
    /// Directory that was asked for.
    pub release_dir: String,
    /// Every manifest has a verifying signature.
    pub ok: bool,
    /// One row per manifest, in file-name order.
    pub manifests: Vec<ManifestSignStatus>,
    /// Signature findings. Other verify checks are not repeated here.
    pub problems: Vec<VerifyProblem>,
}

impl SignStatusReport {
    /// JSON object: `ok`, `release_dir`, `manifests`, `problems`, `problem_count`.
    #[must_use]
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "ok": self.ok,
            "release_dir": self.release_dir,
            "manifests": self.manifests.iter().map(|row| serde_json::json!({
                "file": row.file,
                "signed": row.signed,
                "verified": row.verified,
                "date": row.date,
                "key_id": row.key_id,
                "manifest_sha256": row.manifest_sha256,
            })).collect::<Vec<_>>(),
            "problems": self.problems.iter().map(|problem| serde_json::json!({
                "manifest": if problem.manifest.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(problem.manifest.clone())
                },
                "check": problem.check,
                "message": problem.message,
            })).collect::<Vec<_>>(),
            "problem_count": self.problems.len(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SignatureDocument {
    format: u32,
    algorithm: String,
    key_id: String,
    public_key: String,
    date: String,
    manifest_sha256: String,
    signature: String,
}

struct SigningKey {
    pair: ring::signature::Ed25519KeyPair,
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SigningKey")
            .field(
                "public_key",
                &self.public_key_text().unwrap_or_else(|err| err.code.to_string()),
            )
            .finish_non_exhaustive()
    }
}

impl SigningKey {
    fn from_key_file(text: &str) -> Result<Self, ChunkError> {
        let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
        if lines.next() != Some(SIGNING_KEY_HEADER) {
            return Err(ChunkError::new(
                "release_key",
                format!("not a signing key (first line must be `{SIGNING_KEY_HEADER}`)"),
            ));
        }
        let der = lines
            .next()
            .and_then(decode_hex)
            .ok_or_else(|| ChunkError::new("release_key", "signing key: the key line is not hex"))?;
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(&der).map_err(|_| {
            ChunkError::new(
                "release_key",
                "signing key: the key document is not Ed25519 PKCS#8",
            )
        })?;
        Ok(Self { pair })
    }

    fn public_key_bytes(&self) -> Result<[u8; 32], ChunkError> {
        use ring::signature::KeyPair as _;
        self.pair
            .public_key()
            .as_ref()
            .try_into()
            .map_err(|_| ChunkError::new("release_key", "Ed25519 public key is not 32 bytes"))
    }

    fn public_key_text(&self) -> Result<String, ChunkError> {
        Ok(format!("ed25519:{}", hex_encode(&self.public_key_bytes()?)))
    }

    fn key_id(&self) -> Result<String, ChunkError> {
        Ok(key_id_of(&self.public_key_bytes()?))
    }

    fn sign_payload(&self, payload: &[u8]) -> String {
        let signature = self.pair.sign(&signed_message(payload));
        hex_encode(signature.as_ref())
    }
}

/// Key-file text for a throwaway Ed25519 key. The caller writes it outside the project.
///
/// `regia release sign` does not call this. Tests do, and only into a temp directory.
///
/// # Errors
///
/// `release_key` when the system random source fails.
pub fn generate_throwaway_signing_key() -> Result<String, ChunkError> {
    let rng = ring::rand::SystemRandom::new();
    let document = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| ChunkError::new("release_key", "the system random source failed"))?;
    Ok(format!(
        "{SIGNING_KEY_HEADER}\n{}\n",
        hex_encode(document.as_ref())
    ))
}

/// Sign each release manifest in `dir`.
///
/// The date is `YYYY-MM-DD` and is not read from a clock. `project`, when set, refuses a
/// key whose path is inside that directory before the key file is read.
///
/// # Errors
///
/// `content_key_in_project`, `release_dir`, `release_date`, `release_key`, `release_manifest`,
/// or `io_error`.
pub fn sign_release(
    dir: &Path,
    key: &Path,
    date: &str,
    project: Option<&Path>,
) -> Result<SignReport, ChunkError> {
    if !valid_release_date(date) {
        return Err(ChunkError::new(
            "release_date",
            format!("date {date:?} is not YYYY-MM-DD"),
        ));
    }
    if let Some(project) = project
        && key_is_inside(key, project)?
    {
        return Err(ChunkError::new(
            "content_key_in_project",
            format!(
                "the private key {} is inside the project directory {}",
                display_path(key),
                display_path(project)
            ),
        ));
    }
    if !dir.is_dir() {
        return Err(ChunkError::new(
            "release_dir",
            format!("release directory {} does not exist", dir.display()),
        ));
    }
    let manifests = list_manifests(dir)?;
    if manifests.is_empty() {
        return Err(ChunkError::new(
            "release_manifest",
            format!("no manifest to sign in {}", dir.display()),
        ));
    }
    let key_text = fs::read_to_string(key).map_err(|err| ChunkError::io(key, &err))?;
    let signing = SigningKey::from_key_file(&key_text)?;
    let mut files = Vec::new();
    for manifest in &manifests {
        let name = file_name(manifest)?;
        let bytes = fs::read(manifest).map_err(|err| ChunkError::io(manifest, &err))?;
        let digest = sha256_hex(&bytes);
        let payload = signature_payload(&name, &digest, date);
        let document = SignatureDocument {
            format: RELEASE_SIGNATURE_FORMAT,
            algorithm: "ed25519".to_string(),
            key_id: signing.key_id()?,
            public_key: signing.public_key_text()?,
            date: date.to_string(),
            manifest_sha256: digest,
            signature: signing.sign_payload(&payload),
        };
        let mut encoded = serde_json::to_vec_pretty(&document)
            .map_err(|err| ChunkError::new("io_error", format!("cannot encode signature: {err}")))?;
        encoded.push(b'\n');
        let sig_path = signature_path(manifest);
        write_atomic(&sig_path, &encoded)?;
        files.push(name);
    }
    let mut problems = Vec::new();
    check_signatures(
        dir,
        &manifests,
        &[],
        &VerifyOptions {
            require_signature: true,
            ..VerifyOptions::default()
        },
        &mut problems,
    );
    if !problems.is_empty() {
        for manifest in &manifests {
            let _ = fs::remove_file(signature_path(manifest));
        }
        return Err(ChunkError::new(
            "manifest_signature",
            problems
                .iter()
                .map(|problem| problem.message.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }
    Ok(SignReport { files })
}

/// Signature status of the manifests in `dir`. Does not modify the directory.
#[must_use]
pub fn sign_status(dir: &Path) -> SignStatusReport {
    let release_dir = dir.display().to_string();
    if !dir.is_dir() {
        return SignStatusReport {
            release_dir,
            ok: false,
            manifests: Vec::new(),
            problems: vec![problem(
                "release_dir",
                format!("release directory {} does not exist", dir.display()),
                "",
            )],
        };
    }
    let paths = match list_manifests(dir) {
        Ok(paths) => paths,
        Err(err) => {
            return SignStatusReport {
                release_dir,
                ok: false,
                manifests: Vec::new(),
                problems: vec![problem("release_dir", err.message, "")],
            };
        }
    };
    if paths.is_empty() {
        return SignStatusReport {
            release_dir,
            ok: false,
            manifests: Vec::new(),
            problems: vec![problem(
                "release_manifest",
                format!("no manifest-*.json files in {}", dir.display()),
                "",
            )],
        };
    }
    let mut problems = Vec::new();
    check_signatures(
        dir,
        &paths,
        &[],
        &VerifyOptions {
            require_signature: true,
            ..VerifyOptions::default()
        },
        &mut problems,
    );
    let manifests = paths
        .iter()
        .filter_map(|path| status_row(path, &problems))
        .collect::<Vec<_>>();
    let ok = !manifests.is_empty() && manifests.iter().all(|row| row.verified) && problems.is_empty();
    SignStatusReport {
        release_dir,
        ok,
        manifests,
        problems,
    }
}

/// Check detached signatures for `manifests` that are not in `skipped` (file names).
pub(crate) fn check_signatures(
    _dir: &Path,
    manifests: &[PathBuf],
    skipped: &[String],
    opts: &VerifyOptions,
    problems: &mut Vec<VerifyProblem>,
) {
    for manifest in manifests {
        let Ok(name) = file_name(manifest) else {
            continue;
        };
        if skipped.iter().any(|skipped_name| skipped_name == &name) {
            continue;
        }
        let sig_path = signature_path(manifest);
        if !sig_path.is_file() {
            if opts.require_signature {
                problems.push(problem(
                    "manifest_signature",
                    format!("signature required but {name}.sig is absent"),
                    &name,
                ));
            }
            continue;
        }
        check_one(manifest, &name, &sig_path, opts, problems);
    }
}

fn check_one(
    manifest: &Path,
    name: &str,
    sig_path: &Path,
    opts: &VerifyOptions,
    problems: &mut Vec<VerifyProblem>,
) {
    let document = match read_signature(sig_path) {
        Ok(document) => document,
        Err(message) => {
            problems.push(problem("manifest_signature", message, name));
            return;
        }
    };
    if document.format != RELEASE_SIGNATURE_FORMAT || document.algorithm != "ed25519" {
        problems.push(problem(
            "manifest_signature",
            format!("{name}.sig is not an ed25519 release signature"),
            name,
        ));
        return;
    }
    if !valid_release_date(&document.date) {
        problems.push(problem(
            "manifest_signature",
            format!("{name}.sig date {:?} is not YYYY-MM-DD", document.date),
            name,
        ));
        return;
    }
    let Some(public_key) = parse_public_key(&document.public_key) else {
        problems.push(problem(
            "manifest_signature",
            format!("{name}.sig public key is not ed25519:<64 hex digits>"),
            name,
        ));
        return;
    };
    if key_id_of(&public_key) != document.key_id {
        problems.push(problem(
            "manifest_signature",
            format!("{name}.sig key id does not match its public key"),
            name,
        ));
        return;
    }
    let Some(signature) = decode_fixed::<64>(&document.signature) else {
        problems.push(problem(
            "manifest_signature",
            format!("{name}.sig signature is not 128 hex digits"),
            name,
        ));
        return;
    };
    let payload = signature_payload(name, &document.manifest_sha256, &document.date);
    if ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key.as_ref())
        .verify(&signed_message(&payload), &signature)
        .is_err()
    {
        problems.push(problem(
            "manifest_signature",
            format!("{name}.sig does not match the signed manifest hash and date"),
            name,
        ));
        return;
    }
    let current = match sha256_file(manifest) {
        Ok(digest) => hex_encode(&digest),
        Err(err) => {
            problems.push(problem("manifest_signature", err.message, name));
            return;
        }
    };
    if current != document.manifest_sha256 {
        problems.push(problem(
            "manifest_signature",
            format!("{name} was modified after it was signed"),
            name,
        ));
        return;
    }
    if !opts.trusted_keys.is_empty() && !trusted(&document.public_key, &opts.trusted_keys) {
        problems.push(problem(
            "manifest_signature",
            format!("{name}.sig is signed by a key this release does not trust"),
            name,
        ));
    }
}

fn status_row(manifest: &Path, problems: &[VerifyProblem]) -> Option<ManifestSignStatus> {
    let name = file_name(manifest).ok()?;
    let signed = signature_path(manifest).is_file();
    let verified = signed && !problems.iter().any(|problem| problem.manifest == name);
    let document = read_signature(&signature_path(manifest)).ok();
    let manifest_sha256 = sha256_file(manifest)
        .map(|digest| hex_encode(&digest))
        .unwrap_or_default();
    Some(ManifestSignStatus {
        file: name,
        signed,
        verified,
        date: document.as_ref().map(|doc| doc.date.clone()).unwrap_or_default(),
        key_id: document.map(|doc| doc.key_id).unwrap_or_default(),
        manifest_sha256,
    })
}

fn read_signature(path: &Path) -> Result<SignatureDocument, String> {
    let meta = fs::metadata(path).map_err(|err| format!("{}: {err}", path.display()))?;
    if meta.len() > MAX_SIGNATURE_BYTES {
        return Err(format!(
            "{} is larger than {MAX_SIGNATURE_BYTES} bytes",
            path.display()
        ));
    }
    let bytes = fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|err| format!("{} is not a signature document: {err}", path.display()))
}

/// Serde field list of the `.sig` sidecar for `cargo xtask check-formats` (ADR-8846). Field
/// order is the struct order: a field added, renamed or moved changes it.
#[must_use]
pub fn signature_schema_fingerprint() -> &'static str {
    "format,algorithm,key_id,public_key,date,manifest_sha256,signature"
}

/// The `format` of a `.sig` sidecar, read before the typed parse (`regia upgrade-project`).
///
/// # Errors
///
/// `manifest_signature` when the bytes are not a JSON object with an unsigned `format`.
pub fn peek_signature_version(bytes: &[u8]) -> Result<u32, ChunkError> {
    #[derive(Deserialize)]
    struct Probe {
        format: u32,
    }
    serde_json::from_slice::<Probe>(bytes)
        .map(|probe| probe.format)
        .map_err(|err| ChunkError::new("manifest_signature", format!("not a release signature: {err}")))
}

/// Parse a `.sig` sidecar, check its shape, and write it back the way `regia release sign`
/// writes it: pretty JSON, fields in struct order, a final newline.
///
/// The shape checks are the ones `regia release verify` makes before it looks at the manifest:
/// `format` 1, `algorithm` `ed25519`, a `YYYY-MM-DD` date, `ed25519:` plus 64 hex digits, the
/// key id of that key, 64 hex digits of SHA-256 and 128 hex digits of signature. The signature
/// itself is not checked here: that needs the manifest.
///
/// # Errors
///
/// `manifest_signature` for a document that is too large, does not parse, has an unknown
/// field, or fails a shape check; `release_signature_newer` for a `format` above
/// [`RELEASE_SIGNATURE_FORMAT`].
pub fn canonical_signature_json(bytes: &[u8]) -> Result<Vec<u8>, ChunkError> {
    let bad = |message: String| ChunkError::new("manifest_signature", message);
    if bytes.len() as u64 > MAX_SIGNATURE_BYTES {
        return Err(bad(format!("a signature is at most {MAX_SIGNATURE_BYTES} bytes")));
    }
    let version = peek_signature_version(bytes)?;
    if version > RELEASE_SIGNATURE_FORMAT {
        return Err(ChunkError::new(
            "release_signature_newer",
            format!(
                "release signature format {version} is newer than this build (reads {RELEASE_SIGNATURE_FORMAT})"
            ),
        ));
    }
    let document: SignatureDocument =
        serde_json::from_slice(bytes).map_err(|err| bad(format!("not a signature document: {err}")))?;
    if document.format != RELEASE_SIGNATURE_FORMAT || document.algorithm != "ed25519" {
        return Err(bad("not an ed25519 release signature".to_string()));
    }
    if !valid_release_date(&document.date) {
        return Err(bad(format!("date {:?} is not YYYY-MM-DD", document.date)));
    }
    let Some(public_key) = parse_public_key(&document.public_key) else {
        return Err(bad("public key is not ed25519:<64 hex digits>".to_string()));
    };
    if document.key_id != key_id_of(&public_key) {
        return Err(bad("key id does not match its public key".to_string()));
    }
    if decode_fixed::<32>(&document.manifest_sha256).is_none() {
        return Err(bad("manifest_sha256 is not 64 hex digits".to_string()));
    }
    if decode_fixed::<64>(&document.signature).is_none() {
        return Err(bad("signature is not 128 hex digits".to_string()));
    }
    let mut out = serde_json::to_vec_pretty(&document)
        .map_err(|err| ChunkError::new("io_error", format!("cannot encode signature: {err}")))?;
    out.push(b'\n');
    Ok(out)
}

/// The sidecar `sign_release` writes for `manifest` named `file`, signed with the Ed25519 key
/// of `seed` (tests only: the registry example and corpus fixture are built this way).
#[cfg(test)]
pub(crate) fn signature_for_seed(seed: &[u8; 32], file: &str, manifest: &[u8], date: &str) -> Vec<u8> {
    use ring::signature::KeyPair;
    let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(seed).expect("seed");
    let public: [u8; 32] = pair.public_key().as_ref().try_into().expect("32 bytes");
    let digest = sha256_hex(manifest);
    let payload = signature_payload(file, &digest, date);
    let document = SignatureDocument {
        format: RELEASE_SIGNATURE_FORMAT,
        algorithm: "ed25519".to_string(),
        key_id: key_id_of(&public),
        public_key: format!("ed25519:{}", hex_encode(&public)),
        date: date.to_string(),
        manifest_sha256: digest,
        signature: hex_encode(pair.sign(&signed_message(&payload)).as_ref()),
    };
    let mut out = serde_json::to_vec_pretty(&document).expect("encode");
    out.push(b'\n');
    out
}

fn signature_payload(file: &str, manifest_sha256: &str, date: &str) -> Vec<u8> {
    format!("file {file}\nsha256 {manifest_sha256}\ndate {date}\n").into_bytes()
}

fn signed_message(payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(RELEASE_SIGNATURE_DOMAIN.len() + payload.len());
    message.extend_from_slice(RELEASE_SIGNATURE_DOMAIN);
    message.extend_from_slice(payload);
    message
}

fn signature_path(manifest: &Path) -> PathBuf {
    let mut name = manifest.file_name().unwrap_or_default().to_os_string();
    name.push(".sig");
    manifest.with_file_name(name)
}

fn file_name(path: &Path) -> Result<String, ChunkError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .ok_or_else(|| ChunkError::new("release_manifest", format!("{} has no file name", path.display())))
}

fn key_id_of(public_key: &[u8; 32]) -> String {
    let digest = blake3::hash(public_key);
    hex_encode(&digest.as_bytes()[..8])
}

fn parse_public_key(text: &str) -> Option<[u8; 32]> {
    let hex = text.trim().strip_prefix("ed25519:")?;
    decode_fixed::<32>(&hex.to_ascii_lowercase())
}

fn trusted(public_key: &str, allowed: &[String]) -> bool {
    let want = public_key.to_ascii_lowercase();
    allowed.iter().any(|key| key.trim().to_ascii_lowercase() == want)
}

/// `YYYY-MM-DD`, including a leap day. No clock is read.
fn valid_release_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    if !bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return false;
    }
    let Ok(year) = date[0..4].parse::<u16>() else {
        return false;
    };
    let Ok(month) = date[5..7].parse::<u8>() else {
        return false;
    };
    let Ok(day) = date[8..10].parse::<u8>() else {
        return false;
    };
    if year < 1970 {
        return false;
    }
    let max = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => 29,
        2 => 28,
        _ => return false,
    };
    (1..=max).contains(&day)
}

fn key_is_inside(key: &Path, project: &Path) -> Result<bool, ChunkError> {
    let key_path = located(key)?;
    let project_path = located(project)?;
    Ok(key_path.starts_with(&project_path))
}

fn located(path: &Path) -> Result<PathBuf, ChunkError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|err| ChunkError::new("io_error", format!("current directory: {err}")))?
            .join(path)
    };
    Ok(absolute.canonicalize().unwrap_or(absolute))
}

fn display_path(path: &Path) -> String {
    located(path)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn problem(check: &'static str, message: impl Into<String>, manifest: &str) -> VerifyProblem {
    VerifyProblem {
        check,
        message: message.into(),
        manifest: manifest.to_string(),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ChunkError> {
    let tmp_name = format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|name| name.to_str()).unwrap_or("part"),
        std::process::id()
    );
    let tmp = path.with_file_name(tmp_name);
    {
        let mut file = fs::File::create(&tmp).map_err(|err| ChunkError::io(&tmp, &err))?;
        file.write_all(bytes).map_err(|err| ChunkError::io(&tmp, &err))?;
        file.sync_all().map_err(|err| ChunkError::io(&tmp, &err))?;
    }
    fs::rename(&tmp, path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        ChunkError::io(path, &err)
    })
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    decode_bytes(text)
}

fn decode_fixed<const N: usize>(text: &str) -> Option<[u8; N]> {
    let bytes = decode_bytes(text)?;
    bytes.try_into().ok()
}

fn decode_bytes(text: &str) -> Option<Vec<u8>> {
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let (pairs, rest) = text.as_bytes().as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|[hi, lo]| Some((digit(*hi)? << 4) | digit(*lo)?))
        .collect()
}
