// SPDX-License-Identifier: MPL-2.0
//! The server's static key: [`ServerIdentity`] (private) and [`ServerKey`] (what a client pins).
//!
//! A dedicated server keeps its identity across restarts in a `server.key` file so pins stay
//! valid; a listen server gets a new one per run. The private half never reaches `Debug`, serde or
//! the agent protocol.

use std::path::Path;

use serde::{Deserialize, Serialize};
use snow::params::DHChoice;
use snow::resolvers::{CryptoResolver, DefaultResolver};

use super::{CryptoError, os_random};
use crate::wire::constant_time_eq;

/// Bytes of an X25519 key.
pub const KEY_BYTES: usize = 32;
/// `format_version` of a `server.key` file.
pub const SERVER_KEY_FORMAT_VERSION: u32 = 1;

/// The fields of a `server.key` file (`docs/formats/server-key.md`); the shape
/// `crate::formats::server_key_schema_fingerprint` hashes.
#[derive(Deserialize)]
pub(crate) struct ServerKeyFile {
    pub(crate) format_version: u32,
    pub(crate) private_key: String,
    pub(crate) public_key: ServerKey,
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a String cannot fail.
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn unhex(text: &str) -> Option<[u8; KEY_BYTES]> {
    let bytes = text.as_bytes();
    if bytes.len() != KEY_BYTES * 2 {
        return None;
    }
    let digit = |b: u8| char::from(b).to_digit(16).map(|d| d as u8);
    let mut out = [0u8; KEY_BYTES];
    for (slot, pair) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
        *slot = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(out)
}

/// A server's public key. Text form: 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ServerKey(pub [u8; KEY_BYTES]);

impl ServerKey {
    /// Parses the text form; upper-case digits are accepted.
    pub fn parse(text: &str) -> Option<Self> {
        unhex(text.trim()).map(Self)
    }

    /// True when `other` is the same key, in constant time.
    pub fn matches(&self, other: &[u8]) -> bool {
        constant_time_eq(&self.0, other)
    }
}

impl std::fmt::Display for ServerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex(&self.0))
    }
}

impl std::fmt::Debug for ServerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ServerKey({self})")
    }
}

impl Serialize for ServerKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ServerKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Self::parse(&text).ok_or_else(|| serde::de::Error::custom("a server key is 64 hex digits"))
    }
}

/// A server's static key pair.
#[derive(Clone)]
pub struct ServerIdentity {
    private: [u8; KEY_BYTES],
    public: ServerKey,
}

impl std::fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ServerIdentity {{ public: {}, private: <hidden> }}",
            self.public
        )
    }
}

impl PartialEq for ServerIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.public == other.public
    }
}

impl ServerIdentity {
    /// The pair for `private`. `None` when the curve code is unavailable.
    pub fn from_private(private: [u8; KEY_BYTES]) -> Option<Self> {
        let mut dh = DefaultResolver.resolve_dh(&DHChoice::Curve25519)?;
        dh.set(&private);
        let public: [u8; KEY_BYTES] = dh.pubkey().try_into().ok()?;
        Some(Self {
            private,
            public: ServerKey(public),
        })
    }

    /// A new key pair from the operating system's random source.
    pub fn generate() -> Result<Self, CryptoError> {
        os_random::<KEY_BYTES>()
            .and_then(Self::from_private)
            .ok_or(CryptoError::Unavailable)
    }

    /// The pair a server derives from its secret when it was given no identity, so fixed-secret
    /// tests are deterministic.
    pub fn derived(secret: &[u8; KEY_BYTES]) -> Option<Self> {
        let private = blake3::derive_key("regia_net server identity v1", secret);
        Self::from_private(private)
    }

    /// The public key.
    pub fn public(&self) -> ServerKey {
        self.public
    }

    /// The scalar. Visible inside `crypto` (player proofs and binding proofs). Not a public export.
    pub(in crate::crypto) fn private_bytes(&self) -> &[u8; KEY_BYTES] {
        &self.private
    }

    /// Loads `path`, or creates it with a new key when it does not exist. The file is private
    /// to its owner (mode 0600 on Unix, an owner/`SYSTEM`/`Administrators` DACL on Windows),
    /// and a file other local accounts can access is refused: that key is no longer private,
    /// and quietly using it would make a pin worthless.
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        // The load reads the same verbatim path the writer used, so a destination past
        // `MAX_PATH` is found instead of looking missing and minting a fresh key over it
        // (audit fixrev-15-k net2-platform M6).
        let vpath = crate::private_file::verbatim(path).map_err(|e| format!("{}: {e}", path.display()))?;
        match std::fs::metadata(&vpath) {
            Ok(meta) => {
                refuse_if_exposed(&vpath, path, &meta)?;
                let text = std::fs::read_to_string(&vpath).map_err(|e| format!("{}: {e}", path.display()))?;
                Self::from_file_text(&text).map_err(|e| format!("{}: {e}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let identity = Self::generate().map_err(|e| format!("{}: {e}", path.display()))?;
                identity
                    .write_file(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                Ok(identity)
            }
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    fn file_text(&self) -> String {
        format!(
            "(\n    format_version: {SERVER_KEY_FORMAT_VERSION},\n    private_key: \"{}\",\n    public_key: \"{}\",\n)\n",
            hex(&self.private),
            self.public
        )
    }

    /// Reads the text of a `server.key` file. A newer `format_version` is refused, and the public
    /// key must match the private one.
    pub fn from_file_text(text: &str) -> Result<Self, String> {
        let file: ServerKeyFile = ron::from_str(text).map_err(|e| e.to_string())?;
        if file.format_version > SERVER_KEY_FORMAT_VERSION {
            return Err(format!(
                "format_version {} is newer than {SERVER_KEY_FORMAT_VERSION}",
                file.format_version
            ));
        }
        let private = unhex(&file.private_key).ok_or("private_key is not 64 hex digits")?;
        let identity = Self::from_private(private).ok_or("the curve code is unavailable")?;
        if identity.public != file.public_key {
            return Err("public_key does not belong to private_key".into());
        }
        Ok(identity)
    }

    /// Writes the file atomically (a unique temporary name in the same directory, then a
    /// rename); the temporary file already has its final, owner-only permissions. On Windows
    /// both paths are verbatim so a long destination is not pushed over `MAX_PATH` by the
    /// temporary suffix.
    fn write_file(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        let dest = crate::private_file::verbatim(path)?;
        let (tmp, mut f) = crate::private_file::create_private_temp(path)?;
        let written = f
            .write_all(self.file_text().as_bytes())
            .and_then(|()| f.sync_all());
        drop(f);
        let result = written.and_then(|()| std::fs::rename(&tmp, &dest));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

/// Refused when the key file grants other local accounts any access: a group/other mode bit
/// on Unix, an access-allowed ACE for any principal but the owner, `SYSTEM` and
/// `Administrators` on Windows (the same class of hole; ADR-4828, hardened per audit
/// fixrev-15-k net2-platform H3). `check` is the path the DACL is read from (verbatim on
/// Windows, so a long path reads correctly); `display` names the path in the error.
fn refuse_if_exposed(check: &Path, display: &Path, meta: &std::fs::Metadata) -> Result<(), String> {
    if crate::private_file::exposed_to_other_users(check, meta)? {
        return Err(format!(
            "{}: other users can access this key file; replace it with a freshly generated key (on Unix: chmod 600 first)",
            display.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_text_round_trips_and_rejects_bad_input() {
        let key = ServerKey([0xAB; KEY_BYTES]);
        let text = key.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(ServerKey::parse(&text), Some(key));
        assert_eq!(ServerKey::parse(&text.to_uppercase()), Some(key));
        assert_eq!(ServerKey::parse(&format!(" {text}\n")), Some(key));
        assert_eq!(ServerKey::parse(&text[1..]), None);
        assert_eq!(ServerKey::parse(&"zz".repeat(32)), None);
        assert_eq!(ServerKey::parse(""), None);
        // Non-ASCII of the right byte length must fail, not split a character.
        assert_eq!(ServerKey::parse(&"é".repeat(32)), None);
    }

    #[test]
    fn x25519_known_answer() {
        // RFC 7748 § 6.1, Alice's key pair: proves the resolver really is X25519.
        let private = unhex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a").expect("hex");
        let public = unhex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a").expect("hex");
        let identity = ServerIdentity::from_private(private).expect("curve");
        assert_eq!(identity.public(), ServerKey(public));
    }

    #[test]
    fn debug_output_hides_the_private_key() {
        let identity = ServerIdentity::from_private([0x11; KEY_BYTES]).expect("curve");
        let debug = format!("{identity:?}");
        assert!(!debug.contains(&hex(&[0x11; KEY_BYTES])), "{debug}");
        assert!(debug.contains("hidden"));
    }

    #[test]
    fn derived_identity_is_stable_per_secret() {
        let a = ServerIdentity::derived(&[1; KEY_BYTES]).expect("curve");
        let b = ServerIdentity::derived(&[1; KEY_BYTES]).expect("curve");
        let c = ServerIdentity::derived(&[2; KEY_BYTES]).expect("curve");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    /// Audit wave 6 (netplug H1): a key file its group can read (0640, 0660) was loaded; only
    /// the other-users bits were checked. Group and other bits are both refused now, and the
    /// 0600 file the writer makes still loads.
    #[cfg(unix)]
    #[test]
    fn a_key_file_its_group_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-net-key-mode-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("server.key");
        let made = ServerIdentity::load_or_create(&path).expect("create");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(ServerIdentity::load_or_create(&path).expect("load"), made);
        for refused in [0o640, 0o660, 0o604, 0o610, 0o620] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(refused)).expect("chmod");
            let error = ServerIdentity::load_or_create(&path).expect_err("refused");
            assert!(error.contains("other users can access"), "{refused:o}: {error}");
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).expect("chmod");
        assert_eq!(
            ServerIdentity::load_or_create(&path).expect("owner read-only loads"),
            made
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sprint audit G (net2-platform) M2: on Windows the key file is created with an
    /// owner-only DACL; granting `Users` read back must make `load_or_create` refuse, not
    /// quietly hand out the key.
    #[cfg(windows)]
    #[test]
    fn a_windows_key_file_readable_by_other_users_is_refused() {
        let dir = std::env::temp_dir().join(format!("regia-net-key-acl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("server.key");
        ServerIdentity::load_or_create(&path).expect("create");
        let meta = std::fs::metadata(&path).expect("meta");
        assert!(
            !crate::private_file::exposed_to_other_users(&path, &meta).expect("dacl read"),
            "a fresh key file grants other users access"
        );
        crate::private_file::grant_well_known_for_test(
            &path,
            windows_sys::Win32::Security::WinBuiltinUsersSid,
            windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ,
        )
        .expect("grant Users read");
        let meta = std::fs::metadata(&path).expect("meta");
        assert!(
            crate::private_file::exposed_to_other_users(&path, &meta).expect("dacl read"),
            "the grant was not applied"
        );
        let error = ServerIdentity::load_or_create(&path).expect_err("refused");
        assert!(error.contains("other users can access"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sprint audit G (net2-platform) L3: a destination near `MAX_PATH` must still get its
    /// key; the short temporary name beside it must not push the create over the limit.
    #[cfg(windows)]
    #[test]
    fn a_long_windows_path_still_creates_the_key() {
        let dir = std::env::temp_dir().join(format!(
            "regia-net-key-long-{}-{}",
            std::process::id(),
            "a".repeat(200)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("server.key");
        assert!(
            path.as_os_str().len() >= 252,
            "the test path must be near MAX_PATH: {}",
            path.display()
        );
        let made = ServerIdentity::load_or_create(&path).expect("create");
        assert_eq!(
            ServerIdentity::load_or_create(&path).expect("load").public(),
            made.public()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit net1 (platform): on Windows the privacy wall is the DACL, not mode bits. A key file
    /// that grants read to Users, Everyone or Authenticated Users is refused, like a Unix key
    /// with group/other bits — they are principals outside the owner/SYSTEM/Administrators
    /// allowlist; a freshly created key (that set only) loads.
    #[cfg(windows)]
    #[test]
    fn a_key_file_other_users_can_read_is_refused() {
        use windows_sys::Win32::Security::{WinAuthenticatedUserSid, WinBuiltinUsersSid, WinWorldSid};
        use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-net-key-acl-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("server.key");
        let made = ServerIdentity::load_or_create(&path).expect("create");
        assert_eq!(
            ServerIdentity::load_or_create(&path).expect("the owner-only key loads"),
            made
        );
        for (sid, who) in [
            (WinBuiltinUsersSid, "Users"),
            (WinWorldSid, "Everyone"),
            (WinAuthenticatedUserSid, "Authenticated Users"),
        ] {
            let error = crate::windows_acl::grant_for_test(&path, sid, FILE_GENERIC_READ)
                .and_then(|()| ServerIdentity::load_or_create(&path).map(|_| ()))
                .expect_err("a readable key is refused");
            assert!(error.contains("other users can access"), "{who}: {error}");
            // Back to the owner-only DACL for the next grant: recreate the key file.
            std::fs::remove_file(&path).expect("remove");
            let _ = ServerIdentity::load_or_create(&path).expect("fresh private key");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// corr-low (corr-net LOW): the key was written through the fixed temporary name
    /// `.{pid}.tmp`, created exclusively. A crash between the create and the rename left that
    /// file behind, and a server that restarts with the same PID (PID 1 in a container) could
    /// then never create its key: every start failed with "file exists". A stale temporary
    /// must not block the key.
    #[test]
    fn a_stale_temporary_from_a_crash_does_not_block_the_key() {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-net-key-stale-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("server.key");
        // What a crashed earlier run with this PID leaves next to the key.
        let stale = crate::private_file::temp_path(&path, &std::process::id().to_string());
        std::fs::write(&stale, "half a key").expect("stale temporary");
        let made =
            ServerIdentity::load_or_create(&path).expect("the key is created despite the stale temporary");
        assert_eq!(ServerIdentity::load_or_create(&path).expect("reload"), made);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_text_round_trips_and_refuses_damage() {
        let identity = ServerIdentity::generate().expect("random");
        let text = identity.file_text();
        assert_eq!(ServerIdentity::from_file_text(&text).expect("read"), identity);
        let newer = text.replace("format_version: 1", "format_version: 2");
        assert!(ServerIdentity::from_file_text(&newer).is_err());
        let other = ServerIdentity::generate().expect("random");
        let swapped = text.replace(&identity.public().to_string(), &other.public().to_string());
        assert!(ServerIdentity::from_file_text(&swapped).is_err());
        assert!(ServerIdentity::from_file_text("not ron").is_err());
    }
}
