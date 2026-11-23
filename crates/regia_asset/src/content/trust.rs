// SPDX-License-Identifier: MPL-2.0
//! Signed content manifests (ADR-2710, spec 40 § 11).
//!
//! A manifest fetched from a host carries the BLAKE3 hash of every chunk, so whoever controls the
//! manifest controls the content. TLS authenticates the connection, not the publisher: a CDN, a
//! mirror or a compromised static host can still serve any manifest. A project that lists trusted
//! Ed25519 public keys (`Regia.toml` `[content] trusted_keys`, at most [`MAX_TRUSTED_KEYS`] so a
//! key can be rotated) has the cook write them to [`CONTENT_TRUST_FILE`] next to the manifest; the
//! runtime then checks the detached signature [`CONTENT_SIGNATURE_FILE`] that sits next to the
//! hosted manifest **before** it parses or adopts the manifest. On failure the running manifest is
//! kept and the error is `content_signature_invalid` or `content_signature_missing`.
//!
//! The signed message is [`SIGNATURE_DOMAIN`] followed by the manifest file's exact bytes (domain
//! separation: a signature over a manifest can never be replayed as a signature over anything
//! else). Ed25519 per RFC 8032, through `ring` (behind the `content-signing` feature; the
//! `content-http` feature turns it on). No manifest format changes: the signature is a separate
//! file, so `content-manifest` stays at its frozen v1.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::ContentError;
use super::manifest::write_atomic;

/// Version of `content-trust.ron` this build reads and writes.
pub const CONTENT_TRUST_FORMAT_VERSION: u32 = 1;
/// File name of the trust record, next to the packaged content manifest.
pub const CONTENT_TRUST_FILE: &str = "content-trust.ron";
/// Version of `content-manifest.ron.sig` this build reads and writes.
pub const CONTENT_SIGNATURE_FORMAT_VERSION: u32 = 1;
/// File name of the detached signature, next to the hosted `content-manifest.ron`.
pub const CONTENT_SIGNATURE_FILE: &str = "content-manifest.ron.sig";
/// Most public keys a project trusts at once (the current key and the next one, for rotation).
pub const MAX_TRUSTED_KEYS: usize = 2;
/// Prefix of the signed message; the manifest's bytes follow it.
pub const SIGNATURE_DOMAIN: &[u8] = b"regia-content-manifest-v1\0";
/// The only signature algorithm.
pub const SIGNATURE_ALGORITHM: &str = "ed25519";
/// Prefix of a public key in `Regia.toml` and `content-trust.ron`: `ed25519:<64 hex digits>`.
pub const PUBLIC_KEY_PREFIX: &str = "ed25519:";
/// First line of a private key file written by `regia assets keygen`.
pub const SIGNING_KEY_HEADER: &str = "regia-content-signing-key v1";
/// Largest signature file read (a real one is about 200 bytes).
pub const MAX_SIGNATURE_BYTES: usize = 4096;

/// The serde field-list fingerprint of `content-trust.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn content_trust_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<ContentTrust>()])
}

/// The serde field-list fingerprint of `content-manifest.ron.sig`.
#[must_use]
pub fn content_signature_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<ContentSignature>()])
}

/// What a missing signature does when keys are trusted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignaturePolicy {
    /// Every fetched manifest must carry a valid signature by a trusted key (the default).
    #[default]
    Required,
    /// A manifest without a signature is accepted (and reported `unsigned`); a signature that is
    /// there must still be valid. For the switch-over while a host starts publishing signatures.
    Optional,
}

impl SignaturePolicy {
    /// `required` or `optional` (the `Regia.toml` spelling and the agent label).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Optional => "optional",
        }
    }

    /// Parses [`Self::label`].
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "required" => Some(Self::Required),
            "optional" => Some(Self::Optional),
            _ => None,
        }
    }
}

/// The contents of `content-trust.ron`: which keys may sign this game's manifests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentTrust {
    /// Format version.
    pub format_version: u32,
    /// What a manifest without a signature does.
    pub policy: SignaturePolicy,
    /// Trusted public keys, `ed25519:<64 hex digits>`; one or two.
    pub keys: Vec<String>,
}

impl ContentTrust {
    /// A record trusting `keys` under `policy`.
    ///
    /// # Errors
    /// [`ContentError::BadParams`] for no key, more than [`MAX_TRUSTED_KEYS`], a key that does not
    /// parse, or the same key twice.
    pub fn new(policy: SignaturePolicy, keys: Vec<String>) -> Result<Self, ContentError> {
        let trust = Self {
            format_version: CONTENT_TRUST_FORMAT_VERSION,
            policy,
            keys,
        };
        trust.validate()?;
        Ok(trust)
    }

    /// Checks the key list.
    ///
    /// # Errors
    /// As [`Self::new`].
    pub fn validate(&self) -> Result<(), ContentError> {
        if self.keys.is_empty() {
            return Err(ContentError::BadParams("trusted_keys is empty".into()));
        }
        if self.keys.len() > MAX_TRUSTED_KEYS {
            return Err(ContentError::BadParams(format!(
                "trusted_keys holds {} keys; at most {MAX_TRUSTED_KEYS} (the current key and the next one)",
                self.keys.len()
            )));
        }
        let mut seen = Vec::new();
        for key in &self.keys {
            let bytes = parse_public_key(key).map_err(ContentError::BadParams)?;
            if seen.contains(&bytes) {
                return Err(ContentError::BadParams(format!(
                    "trusted key {key} is listed twice"
                )));
            }
            seen.push(bytes);
        }
        Ok(())
    }

    /// The key ids of the trusted keys ([`key_id`]), in list order.
    #[must_use]
    pub fn key_ids(&self) -> Vec<String> {
        self.keys
            .iter()
            .filter_map(|k| parse_public_key(k).ok())
            .map(|k| key_id(&k))
            .collect()
    }

    /// Reads the record in `dir`; `Ok(None)` when there is none.
    ///
    /// # Errors
    /// The file does not read or parse, is newer than this build, or lists bad keys.
    pub fn read_from_dir(dir: &Path) -> Result<Option<Self>, ContentError> {
        let text = match std::fs::read_to_string(dir.join(CONTENT_TRUST_FILE)) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(ContentError::Io(e.to_string())),
        };
        Self::from_ron_str(&text).map(Some)
    }

    /// Parses the text of a `content-trust.ron`.
    ///
    /// # Errors
    /// The text does not parse, it is newer than this build, or its keys are bad.
    pub fn from_ron_str(text: &str) -> Result<Self, ContentError> {
        let trust: Self =
            ron::from_str(text).map_err(|e| ContentError::Format(format!("{CONTENT_TRUST_FILE}: {e}")))?;
        if trust.format_version > CONTENT_TRUST_FORMAT_VERSION {
            return Err(ContentError::ManifestVersion {
                found: trust.format_version,
                supported: CONTENT_TRUST_FORMAT_VERSION,
            });
        }
        trust
            .validate()
            .map_err(|e| ContentError::Format(format!("{CONTENT_TRUST_FILE}: {e}")))?;
        Ok(trust)
    }

    /// The record as RON text.
    ///
    /// # Errors
    /// RON cannot write it.
    pub fn to_ron_string(&self) -> Result<String, ContentError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| ContentError::Format(e.to_string()))
    }

    /// Writes the record into `dir`, atomically.
    ///
    /// # Errors
    /// The file does not write.
    pub fn write_to_dir(&self, dir: &Path) -> Result<(), ContentError> {
        let text = self.to_ron_string()?;
        write_atomic(&dir.join(CONTENT_TRUST_FILE), text.as_bytes())
    }
}

/// The contents of `content-manifest.ron.sig`: a detached Ed25519 signature over
/// [`SIGNATURE_DOMAIN`] + the manifest's bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentSignature {
    /// Format version.
    pub format_version: u32,
    /// Always [`SIGNATURE_ALGORITHM`].
    pub algorithm: String,
    /// [`key_id`] of the signing key (16 lowercase hex digits).
    pub key_id: String,
    /// The 64-byte signature as 128 lowercase hex digits.
    pub signature: String,
}

impl ContentSignature {
    /// Parses a signature file's bytes (at most [`MAX_SIGNATURE_BYTES`]).
    ///
    /// # Errors
    /// [`ContentError::SignatureInvalid`] for anything that is not a well-formed v1 Ed25519
    /// signature record.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ContentError> {
        let bad = |why: String| ContentError::SignatureInvalid(format!("{CONTENT_SIGNATURE_FILE}: {why}"));
        if bytes.len() > MAX_SIGNATURE_BYTES {
            return Err(bad(format!("larger than {MAX_SIGNATURE_BYTES} bytes")));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| bad("not UTF-8".into()))?;
        let sig: Self = ron::from_str(text).map_err(|e| bad(e.to_string()))?;
        if sig.format_version == 0 || sig.format_version > CONTENT_SIGNATURE_FORMAT_VERSION {
            return Err(bad(format!(
                "format_version {} (this build reads {CONTENT_SIGNATURE_FORMAT_VERSION})",
                sig.format_version
            )));
        }
        if sig.algorithm != SIGNATURE_ALGORITHM {
            return Err(bad(format!(
                "algorithm {:?} is not {SIGNATURE_ALGORITHM}",
                sig.algorithm
            )));
        }
        if sig.key_id.len() != 16 || decode_hex(&sig.key_id).is_none() {
            return Err(bad("key_id is not 16 lowercase hex digits".into()));
        }
        if sig.signature_bytes().is_none() {
            return Err(bad("signature is not 128 lowercase hex digits".into()));
        }
        Ok(sig)
    }

    /// The record as RON text (what `regia assets sign` writes).
    ///
    /// # Errors
    /// RON cannot write it.
    pub fn to_ron_string(&self) -> Result<String, ContentError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| ContentError::Format(e.to_string()))
    }

    /// The 64 signature bytes, or `None` when the hex is malformed.
    #[must_use]
    pub fn signature_bytes(&self) -> Option<[u8; 64]> {
        if self.signature.len() != 128 {
            return None;
        }
        decode_hex(&self.signature)?.try_into().ok()
    }
}

/// The outcome of a signature check on a fetched manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignatureCheck {
    /// The game trusts no keys (no `content-trust.ron`): nothing was checked (TLS only).
    NotConfigured,
    /// Signed by the trusted key with this id.
    Verified {
        /// [`key_id`] of the key that signed it.
        key_id: String,
    },
    /// No signature, accepted because the policy is [`SignaturePolicy::Optional`].
    Unsigned,
}

impl SignatureCheck {
    /// `not_configured`, `verified` or `unsigned` (the agent label).
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Verified { .. } => "verified",
            Self::Unsigned => "unsigned",
        }
    }
}

/// The message a signature covers: [`SIGNATURE_DOMAIN`] then `manifest`.
#[must_use]
pub fn signed_message(manifest: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(SIGNATURE_DOMAIN.len() + manifest.len());
    message.extend_from_slice(SIGNATURE_DOMAIN);
    message.extend_from_slice(manifest);
    message
}

/// The id of a public key: the first 8 bytes of its BLAKE3, as 16 lowercase hex digits.
#[must_use]
pub fn key_id(public_key: &[u8; 32]) -> String {
    encode_hex(&blake3::hash(public_key).as_bytes()[..8])
}

/// `ed25519:<64 hex digits>` for a public key.
#[must_use]
pub fn format_public_key(public_key: &[u8; 32]) -> String {
    format!("{PUBLIC_KEY_PREFIX}{}", encode_hex(public_key))
}

/// Parses `ed25519:<64 hex digits>` (hex case-insensitive, surrounding space ignored).
///
/// # Errors
/// A message naming what is wrong.
pub fn parse_public_key(text: &str) -> Result<[u8; 32], String> {
    let t = text.trim();
    let hex = t.strip_prefix(PUBLIC_KEY_PREFIX).ok_or_else(|| {
        format!("trusted key {t:?} does not start with {PUBLIC_KEY_PREFIX} (write it as `regia assets keygen` prints it)")
    })?;
    let lower = hex.to_ascii_lowercase();
    if lower.len() != 64 {
        return Err(format!(
            "trusted key {t:?} is not 64 hex digits after {PUBLIC_KEY_PREFIX}"
        ));
    }
    decode_hex(&lower)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| format!("trusted key {t:?} is not hex"))
}

/// Checks `manifest` (the hosted file's exact bytes) against `signature` (the bytes of
/// [`CONTENT_SIGNATURE_FILE`], `None` when the host has none) under `trust`. Call it **before**
/// parsing or adopting the manifest.
///
/// # Errors
/// [`ContentError::SignatureMissing`] when keys are trusted, the policy is
/// [`SignaturePolicy::Required`] and there is no signature; [`ContentError::SignatureInvalid`] for a
/// malformed signature, a key that is not trusted, a signature that does not verify, or a build
/// without the `content-signing` feature asked to verify one.
pub fn check_manifest_signature(
    trust: Option<&ContentTrust>,
    manifest: &[u8],
    signature: Option<&[u8]>,
) -> Result<SignatureCheck, ContentError> {
    let Some(trust) = trust else {
        return Ok(SignatureCheck::NotConfigured);
    };
    let Some(signature) = signature else {
        return match trust.policy {
            SignaturePolicy::Required => Err(ContentError::SignatureMissing(format!(
                "the host has no {CONTENT_SIGNATURE_FILE} and this game requires signed manifests"
            ))),
            SignaturePolicy::Optional => {
                tracing::warn!(target: "regia_asset", "content manifest is unsigned (signature policy optional)");
                Ok(SignatureCheck::Unsigned)
            }
        };
    };
    let sig = ContentSignature::from_bytes(signature)?;
    let key = trust
        .keys
        .iter()
        .filter_map(|k| parse_public_key(k).ok())
        .find(|k| key_id(k) == sig.key_id)
        .ok_or_else(|| {
            ContentError::SignatureInvalid(format!(
                "signed by key {}, which this game does not trust (trusted: {})",
                sig.key_id,
                trust.key_ids().join(", ")
            ))
        })?;
    let bytes = sig
        .signature_bytes()
        .ok_or_else(|| ContentError::SignatureInvalid("signature is not hex".into()))?;
    verify_ed25519(&key, &signed_message(manifest), &bytes)?;
    Ok(SignatureCheck::Verified { key_id: sig.key_id })
}

#[cfg(feature = "content-signing")]
fn verify_ed25519(key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> Result<(), ContentError> {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(message, signature)
        .map_err(|_| {
            ContentError::SignatureInvalid(format!(
                "the signature by key {} does not match the manifest",
                key_id(key)
            ))
        })
}

#[cfg(not(feature = "content-signing"))]
fn verify_ed25519(_key: &[u8; 32], _message: &[u8], _signature: &[u8; 64]) -> Result<(), ContentError> {
    Err(ContentError::SignatureInvalid(
        "this build cannot check Ed25519 signatures (build with regia_asset/content-signing or regia/content-http)"
            .into(),
    ))
}

/// A private signing key (`regia assets keygen` / `regia assets sign`).
#[cfg(feature = "content-signing")]
pub struct SigningKey {
    pair: ring::signature::Ed25519KeyPair,
}

#[cfg(feature = "content-signing")]
impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the private half.
        f.debug_struct("SigningKey")
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "content-signing")]
impl SigningKey {
    /// A new random key and its key file text ([`SIGNING_KEY_HEADER`], then the PKCS#8 v2
    /// document in hex). The caller writes the text where only its owner can read it.
    ///
    /// # Errors
    /// The system random source failed.
    pub fn generate() -> Result<(Self, String), ContentError> {
        let rng = ring::rand::SystemRandom::new();
        let doc = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| ContentError::Io("the system random source failed".into()))?;
        let text = format!("{SIGNING_KEY_HEADER}\n{}\n", encode_hex(doc.as_ref()));
        let key = Self::from_key_file(&text)?;
        Ok((key, text))
    }

    /// Reads a key file written by [`Self::generate`].
    ///
    /// # Errors
    /// [`ContentError::Format`] when the text is not a key file.
    pub fn from_key_file(text: &str) -> Result<Self, ContentError> {
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
        if lines.next() != Some(SIGNING_KEY_HEADER) {
            return Err(ContentError::Format(format!(
                "not a content signing key (first line must be `{SIGNING_KEY_HEADER}`)"
            )));
        }
        let der = lines
            .next()
            .and_then(decode_hex)
            .ok_or_else(|| ContentError::Format("content signing key: the key line is not hex".into()))?;
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(&der)
            .map_err(|e| ContentError::Format(format!("content signing key: {e}")))?;
        Ok(Self { pair })
    }

    /// The public key as `ed25519:<64 hex digits>` (what goes into `[content] trusted_keys`).
    #[must_use]
    pub fn public_key(&self) -> String {
        format_public_key(&self.public_key_bytes())
    }

    /// The raw public key.
    #[must_use]
    pub fn public_key_bytes(&self) -> [u8; 32] {
        use ring::signature::KeyPair as _;
        let mut out = [0u8; 32];
        out.copy_from_slice(self.pair.public_key().as_ref());
        out
    }

    /// [`key_id`] of this key.
    #[must_use]
    pub fn key_id(&self) -> String {
        key_id(&self.public_key_bytes())
    }

    /// The detached signature of `manifest` (the file's exact bytes).
    #[must_use]
    pub fn sign_manifest(&self, manifest: &[u8]) -> ContentSignature {
        let sig = self.pair.sign(&signed_message(manifest));
        ContentSignature {
            format_version: CONTENT_SIGNATURE_FORMAT_VERSION,
            algorithm: SIGNATURE_ALGORITHM.into(),
            key_id: self.key_id(),
            signature: encode_hex(sig.as_ref()),
        }
    }
}

/// Lowercase hex of `bytes`.
fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 15)] as char);
    }
    out
}

/// Bytes of lowercase hex text; `None` for an odd length or any other character.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_rejects_junk() {
        assert_eq!(encode_hex(&[0, 0xab, 0xff]), "00abff");
        assert_eq!(decode_hex("00abff"), Some(vec![0, 0xab, 0xff]));
        for bad in ["0", "0g", "AB", "zz", " 0"] {
            assert_eq!(decode_hex(bad), None, "{bad}");
        }
    }

    #[test]
    fn public_keys_parse_and_trust_records_validate() {
        let key = [7u8; 32];
        let text = format_public_key(&key);
        assert_eq!(parse_public_key(&text), Ok(key));
        assert_eq!(
            parse_public_key(&text.to_ascii_uppercase().replace("ED25519:", "ed25519:")),
            Ok(key)
        );
        assert!(parse_public_key("0707").is_err());
        assert!(parse_public_key("ed25519:0707").is_err());
        assert_eq!(key_id(&key).len(), 16);
        let other = format_public_key(&[8u8; 32]);
        assert!(ContentTrust::new(SignaturePolicy::Required, vec![text.clone()]).is_ok());
        assert!(ContentTrust::new(SignaturePolicy::Required, vec![text.clone(), other.clone()]).is_ok());
        assert!(ContentTrust::new(SignaturePolicy::Required, vec![]).is_err());
        assert!(ContentTrust::new(SignaturePolicy::Required, vec![text.clone(), text.clone()]).is_err());
        let third = format_public_key(&[9u8; 32]);
        assert!(ContentTrust::new(SignaturePolicy::Optional, vec![text.clone(), other, third]).is_err());
        let trust = ContentTrust::new(SignaturePolicy::Optional, vec![text]).expect("valid");
        let back = ContentTrust::from_ron_str(&trust.to_ron_string().expect("ron")).expect("parse");
        assert_eq!(back, trust);
    }

    #[test]
    fn without_trust_nothing_is_checked() {
        assert_eq!(
            check_manifest_signature(None, b"anything", None),
            Ok(SignatureCheck::NotConfigured)
        );
    }

    #[test]
    fn a_missing_signature_follows_the_policy() {
        let keys = vec![format_public_key(&[1u8; 32])];
        let required = ContentTrust::new(SignaturePolicy::Required, keys.clone()).expect("valid");
        let err = check_manifest_signature(Some(&required), b"m", None).expect_err("missing");
        assert_eq!(err.reason(), "content_signature_missing");
        let optional = ContentTrust::new(SignaturePolicy::Optional, keys).expect("valid");
        assert_eq!(
            check_manifest_signature(Some(&optional), b"m", None),
            Ok(SignatureCheck::Unsigned)
        );
        // Optional never accepts a broken signature.
        let err = check_manifest_signature(Some(&optional), b"m", Some(b"junk")).expect_err("junk");
        assert_eq!(err.reason(), "content_signature_invalid");
    }

    #[test]
    fn signature_records_reject_every_malformed_field() {
        let good = ContentSignature {
            format_version: 1,
            algorithm: "ed25519".into(),
            key_id: "0123456789abcdef".into(),
            signature: "ab".repeat(64),
        };
        let text = good.to_ron_string().expect("ron");
        assert_eq!(ContentSignature::from_bytes(text.as_bytes()), Ok(good.clone()));
        let variants = [
            ContentSignature {
                format_version: 2,
                ..good.clone()
            },
            ContentSignature {
                format_version: 0,
                ..good.clone()
            },
            ContentSignature {
                algorithm: "rsa".into(),
                ..good.clone()
            },
            ContentSignature {
                key_id: "0123".into(),
                ..good.clone()
            },
            ContentSignature {
                key_id: "0123456789ABCDEF".into(),
                ..good.clone()
            },
            ContentSignature {
                signature: "ab".repeat(63),
                ..good.clone()
            },
            ContentSignature {
                signature: "zz".repeat(64),
                ..good.clone()
            },
        ];
        for bad in variants {
            let text = bad.to_ron_string().expect("ron");
            let err = ContentSignature::from_bytes(text.as_bytes()).expect_err("bad");
            assert_eq!(err.reason(), "content_signature_invalid", "{bad:?}");
        }
        assert!(ContentSignature::from_bytes(&vec![b' '; MAX_SIGNATURE_BYTES + 1]).is_err());
        assert!(ContentSignature::from_bytes(&[0xff, 0xfe]).is_err());
    }
}
