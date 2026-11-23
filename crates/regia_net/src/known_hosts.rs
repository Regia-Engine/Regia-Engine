// SPDX-License-Identifier: MPL-2.0
//! Trust on first use for server keys (ADR-2416): `known_hosts.ron`, a versioned file in the
//! player's user directory that maps a host id to the server key it proved the first time.
//!
//! The model is the one SSH clients use for host keys (RFC 4251 § 4.1, the "known hosts"
//! database): the first connection to a host records its key, every later one must show the same
//! key, and a different key is refused (`net_host_key_changed`) rather than silently replaced. The
//! player or a tool decides what a change means, then trusts the new key or forgets the host.
//!
//! [`KnownHosts`] is the store; `crate::link::LinkOptions::known_hosts` turns it on for a
//! `NetLink` client, and the agent methods `net.known_hosts`, `net.trust` and `net.forget`
//! (`book/agents/net-trust.md`) read and change it. Format: `docs/formats/known-hosts-ron.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::crypto::ServerKey;

/// `format_version` of `known_hosts.ron`.
pub const KNOWN_HOSTS_FORMAT_VERSION: u32 = 1;
/// The store's file name under the user directory (`user://known_hosts.ron`).
pub const KNOWN_HOSTS_FILE: &str = "known_hosts.ron";
/// Hosts one store holds at most; a full store refuses new hosts rather than dropping old pins.
pub const MAX_KNOWN_HOSTS: usize = 4096;
/// Bytes of a host id at most.
pub const MAX_HOST_ID_BYTES: usize = 256;
/// Bytes of a store file read at most.
pub const MAX_KNOWN_HOSTS_FILE_BYTES: u64 = 4 << 20;

/// Damaged copies of the store kept by [`KnownHosts::load_or_recover`] (`known_hosts.ron.bad-1`
/// .. `.bad-5`, ADR-3102).
pub const MAX_BAD_STORE_COPIES: u32 = 5;

/// `<path>.bad-<n>`.
fn bad_copy_path(path: &Path, n: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".bad-{n}"));
    PathBuf::from(name)
}

/// Renames `path` to its next free `<name>.bad-<n>`; when all [`MAX_BAD_STORE_COPIES`] exist, the
/// oldest is dropped and the rest shift down (the rule of `regia_framework::user_file`, ADR-3102;
/// this crate cannot depend on the framework).
fn move_aside(path: &Path) -> std::io::Result<PathBuf> {
    let free = (1..=MAX_BAD_STORE_COPIES)
        .map(|n| bad_copy_path(path, n))
        .find(|candidate| std::fs::symlink_metadata(candidate).is_err());
    let target = match free {
        Some(target) => target,
        None => {
            std::fs::remove_file(bad_copy_path(path, 1))?;
            for n in 2..=MAX_BAD_STORE_COPIES {
                std::fs::rename(bad_copy_path(path, n), bad_copy_path(path, n - 1))?;
            }
            bad_copy_path(path, MAX_BAD_STORE_COPIES)
        }
    };
    std::fs::rename(path, &target)?;
    Ok(target)
}

/// How a host's key came to be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustSource {
    /// Recorded the first time a client connected.
    FirstUse,
    /// Set by the player, a tool or an agent (`net.trust`).
    Manual,
}

impl TrustSource {
    /// Lowercase name for the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FirstUse => "first_use",
            Self::Manual => "manual",
        }
    }
}

/// One trusted host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownHost {
    /// The server key the host must prove.
    pub key: ServerKey,
    /// How it was trusted.
    pub source: TrustSource,
}

/// The fields of a `known_hosts.ron` file; the shape `crate::formats::known_hosts_schema_fingerprint`
/// hashes.
#[derive(Serialize, Deserialize)]
pub(crate) struct KnownHostsFile {
    pub(crate) format_version: u32,
    pub(crate) hosts: BTreeMap<String, KnownHost>,
}

/// What [`KnownHosts::check`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostTrust {
    /// The host was unknown; its key is now pinned (save the store).
    PinnedNow,
    /// The host showed the pinned key.
    Known,
}

/// Why a store operation failed. Each has a lowercase [`KnownHostsError::code`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum KnownHostsError {
    /// The host proved a key that is not the one pinned for it.
    #[error("{host} proved key {offered}, but {pinned} is pinned for it")]
    HostKeyChanged {
        /// The host id.
        host: String,
        /// The pinned key.
        pinned: ServerKey,
        /// The key the host proved.
        offered: ServerKey,
    },
    /// The host id is empty, too long or has control characters.
    #[error("bad host id: {0}")]
    BadHostId(String),
    /// The store holds [`MAX_KNOWN_HOSTS`] hosts.
    #[error("the known-hosts store is full ({MAX_KNOWN_HOSTS} hosts)")]
    Full,
    /// The file could not be read or parsed. It is never overwritten in that state.
    #[error("known hosts file {path}: {why}")]
    Unreadable {
        /// The file.
        path: String,
        /// What went wrong.
        why: String,
    },
    /// The file has a newer `format_version` than this build reads.
    #[error("known hosts file {path}: format_version {found} is newer than {KNOWN_HOSTS_FORMAT_VERSION}")]
    Newer {
        /// The file.
        path: String,
        /// Its version.
        found: u32,
    },
    /// The file could not be written.
    #[error("known hosts file {path}: {why}")]
    WriteFailed {
        /// The file.
        path: String,
        /// What went wrong.
        why: String,
    },
}

impl KnownHostsError {
    /// Lowercase code for logs and the agent protocol.
    pub fn code(&self) -> &'static str {
        match self {
            Self::HostKeyChanged { .. } => "net_host_key_changed",
            Self::BadHostId(_) => "net_bad_host_id",
            Self::Full => "net_known_hosts_full",
            Self::Unreadable { .. } => "net_known_hosts_unreadable",
            Self::Newer { .. } => "net_known_hosts_newer",
            Self::WriteFailed { .. } => "net_known_hosts_write_failed",
        }
    }
}

/// The canonical form of a host id: trimmed and ASCII-lowercased (`Game.Example.com:7777` and
/// `game.example.com:7777` are one host). Any text names a host: an address, a DNS name with its
/// port, a lobby or relay id.
pub fn normalize_host_id(text: &str) -> Result<String, KnownHostsError> {
    let id = text.trim();
    if id.is_empty() {
        return Err(KnownHostsError::BadHostId("empty".into()));
    }
    if id.len() > MAX_HOST_ID_BYTES {
        return Err(KnownHostsError::BadHostId(format!(
            "{} bytes, at most {MAX_HOST_ID_BYTES}",
            id.len()
        )));
    }
    if id.chars().any(char::is_control) {
        return Err(KnownHostsError::BadHostId("control characters".into()));
    }
    Ok(id.to_ascii_lowercase())
}

#[cfg(feature = "ecs")]
/// Trust on first use for one client connection, shared by `NetLink` (ADR-2416) and
/// `NetSession::connect` (ADR-3283). [`FirstUsePin::start`] reads the store before the connection
/// and gives the key to pin in the transport; [`FirstUsePin::on_connected`] records a first-use key
/// after the server proved it, before anything secret is sent.
#[derive(Debug, Clone)]
pub(crate) struct FirstUsePin {
    /// The store.
    path: PathBuf,
    /// The normalized host id.
    host: String,
    /// The store already held a key for the host, and the client pinned it.
    from_store: bool,
    /// The first-use pin was handled.
    done: bool,
}

#[cfg(feature = "ecs")]
impl FirstUsePin {
    /// Reads `path` for `host_id` (normalized). Returns the pin state and the stored key, which
    /// the caller must hand the transport as its pinned server key.
    pub(crate) fn start(path: &Path, host_id: &str) -> Result<(Self, Option<ServerKey>), KnownHostsError> {
        let host = normalize_host_id(host_id)?;
        let store = KnownHosts::load(path)?;
        let pinned = store.get(&host).map(|h| h.key);
        Ok((
            Self {
                path: path.to_path_buf(),
                host,
                from_store: pinned.is_some(),
                done: false,
            },
            pinned,
        ))
    }

    /// The normalized host id.
    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    /// True when the key came from the store (a key mismatch then means the host's key changed).
    pub(crate) fn pinned_by_store(&self) -> bool {
        self.from_store
    }

    /// After the server proved `key`: records it when the host was unknown. Reloads the store
    /// first, so a key another process pinned meanwhile is checked, not overwritten. `Some(saved)`
    /// when this call pinned the key (`saved: false` = the file could not be written, so the pin
    /// lasts for this connection only); `None` when nothing was recorded.
    pub(crate) fn on_connected(&mut self, key: ServerKey) -> Result<Option<bool>, KnownHostsError> {
        if self.done || self.from_store {
            self.done = true;
            return Ok(None);
        }
        self.done = true;
        let mut store = KnownHosts::load(&self.path)?;
        match store.check(&self.host, key)? {
            HostTrust::Known => Ok(None),
            HostTrust::PinnedNow => {
                let saved = match store.save(&self.path) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(target: "regia_net", host = %self.host, error = %e, "known hosts: the first-use pin was not saved");
                        false
                    }
                };
                tracing::info!(target: "regia_net", host = %self.host, %key, saved, "known hosts: trusted the server's key on first use");
                Ok(Some(saved))
            }
        }
    }
}

/// The trusted hosts. Load it, [`KnownHosts::check`] a key, save it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnownHosts {
    hosts: BTreeMap<String, KnownHost>,
}

impl KnownHosts {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads `path`; a missing file is an empty store. A damaged or newer file is an error, and
    /// the caller must not save over it.
    pub fn load(path: &Path) -> Result<Self, KnownHostsError> {
        let unreadable = |why: String| KnownHostsError::Unreadable {
            path: path.display().to_string(),
            why,
        };
        // The load reads the same verbatim path the save used, so a destination past
        // `MAX_PATH` is found instead of reading as an empty store forever
        // (audit fixrev-15-k net2-platform M6).
        let vpath = crate::private_file::verbatim(path).map_err(|e| unreadable(e.to_string()))?;
        match std::fs::metadata(&vpath) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(e) => return Err(unreadable(e.to_string())),
            Ok(meta) if meta.len() > MAX_KNOWN_HOSTS_FILE_BYTES => {
                return Err(unreadable(format!(
                    "larger than {MAX_KNOWN_HOSTS_FILE_BYTES} bytes"
                )));
            }
            Ok(_) => {}
        }
        let text = std::fs::read_to_string(&vpath).map_err(|e| unreadable(e.to_string()))?;
        Self::from_ron(&text).map_err(|e| match e {
            KnownHostsError::Unreadable { why, .. } => unreadable(why),
            KnownHostsError::Newer { found, .. } => KnownHostsError::Newer {
                path: path.display().to_string(),
                found,
            },
            other => other,
        })
    }

    /// [`KnownHosts::load`] for a "reset trusted servers" action (ADR-3102): a damaged or newer
    /// store is renamed to `known_hosts.ron.bad-<n>` (at most [`MAX_BAD_STORE_COPIES`], the oldest
    /// dropped first), one warn line names it, and an empty store comes back, so the next
    /// connection to each host is first use again. Returns the store and the path of the copy
    /// when one was made. [`KnownHosts::load`] itself stays fail-closed (ADR-2416).
    ///
    /// # Errors
    /// A read failure, or a damaged store that cannot be moved aside: it is left as it is and the
    /// caller must not save over it.
    pub fn load_or_recover(path: &Path) -> Result<(Self, Option<PathBuf>), KnownHostsError> {
        let why = match Self::load(path) {
            Ok(store) => return Ok((store, None)),
            Err(e @ (KnownHostsError::Unreadable { .. } | KnownHostsError::Newer { .. })) => e,
            Err(e) => return Err(e),
        };
        // `load` refuses a path it cannot stat with `Unreadable` too; only a file is moved.
        if !std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file()) {
            return Err(why);
        }
        // `load` also reports a file it cannot open (permissions, a Windows sharing violation
        // while a scanner holds it) as `Unreadable`. That is a read failure, not a damaged
        // store: moving it aside would drop every trust pin (corr-net, ADR-5580).
        if std::fs::File::open(path).is_err() {
            return Err(why);
        }
        match move_aside(path) {
            Ok(moved) => {
                tracing::warn!(
                    target: "regia_net",
                    code = why.code(),
                    "known hosts file {} is unusable ({why}); moved it to {} and starting with no trusted hosts",
                    path.display(),
                    moved.display()
                );
                Ok((Self::new(), Some(moved)))
            }
            Err(e) => Err(KnownHostsError::Unreadable {
                path: path.display().to_string(),
                why: format!("{why}; it cannot be moved aside: {e}"),
            }),
        }
    }

    /// Parses the text of a store file. Host ids are normalized; two ids that normalize alike
    /// with different keys, too many hosts or a bad id refuse the file.
    pub fn from_ron(text: &str) -> Result<Self, KnownHostsError> {
        let unreadable = |why: String| KnownHostsError::Unreadable {
            path: String::new(),
            why,
        };
        let file: KnownHostsFile = ron::from_str(text).map_err(|e| unreadable(e.to_string()))?;
        if file.format_version > KNOWN_HOSTS_FORMAT_VERSION {
            return Err(KnownHostsError::Newer {
                path: String::new(),
                found: file.format_version,
            });
        }
        if file.hosts.len() > MAX_KNOWN_HOSTS {
            return Err(unreadable(format!("more than {MAX_KNOWN_HOSTS} hosts")));
        }
        let mut hosts = BTreeMap::new();
        for (id, host) in file.hosts {
            let id = normalize_host_id(&id).map_err(|e| unreadable(e.to_string()))?;
            if hosts
                .insert(id.clone(), host)
                .is_some_and(|old: KnownHost| old.key != host.key)
            {
                return Err(unreadable(format!("{id} is listed twice with different keys")));
            }
        }
        Ok(Self { hosts })
    }

    /// The file text: `format_version`, then the hosts sorted by id.
    pub fn to_ron(&self) -> String {
        // Strings, keys and unit variants always serialize.
        self.ron_text().unwrap_or_default()
    }

    /// [`Self::to_ron`], with a serializer error kept: `save` refuses to replace the file with an
    /// empty one (rc1-rev-net-k 7).
    fn ron_text(&self) -> Result<String, ron::Error> {
        let file = KnownHostsFile {
            format_version: KNOWN_HOSTS_FORMAT_VERSION,
            hosts: self.hosts.clone(),
        };
        // `\n` on every platform: ron's default is `\r\n` on Windows, which made the file and
        // its canonical form differ by host (ADR-3150).
        let config = regia_core::ron_pretty();
        Ok(ron::ser::to_string_pretty(&file, config)? + "\n")
    }

    /// Writes the store to `path` atomically (a unique temporary file, then a rename),
    /// creating the directory. The file is private to its owner (0600 on Unix, an
    /// owner/`SYSTEM`/`Administrators` DACL on Windows): it decides whom the player trusts.
    pub fn save(&self, path: &Path) -> Result<(), KnownHostsError> {
        use std::io::Write;
        let failed = |e: std::io::Error| KnownHostsError::WriteFailed {
            path: path.display().to_string(),
            why: e.to_string(),
        };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            // The directory creation uses the same verbatim path as the file write, so a
            // destination past `MAX_PATH` does not fail here while the file create succeeds
            // (audit fixrev-15-k net2-platform M6).
            let vdir = crate::private_file::verbatim(dir).map_err(failed)?;
            std::fs::create_dir_all(&vdir).map_err(failed)?;
        }
        let dest = crate::private_file::verbatim(path).map_err(failed)?;
        let text = self.ron_text().map_err(|e| KnownHostsError::WriteFailed {
            path: path.display().to_string(),
            why: e.to_string(),
        })?;
        let (tmp, mut f) = crate::private_file::create_private_temp(path).map_err(failed)?;
        let written = f.write_all(text.as_bytes()).and_then(|()| f.sync_all());
        drop(f);
        let result = written.and_then(|()| std::fs::rename(&tmp, &dest));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map_err(failed)
    }

    /// Hosts held.
    pub fn len(&self) -> usize {
        self.hosts.len()
    }

    /// True with no hosts.
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    /// The entry for `host` (any spelling [`normalize_host_id`] accepts).
    pub fn get(&self, host: &str) -> Option<&KnownHost> {
        normalize_host_id(host).ok().and_then(|id| self.hosts.get(&id))
    }

    /// Every host, sorted by id.
    pub fn hosts(&self) -> impl Iterator<Item = (&str, &KnownHost)> {
        self.hosts.iter().map(|(id, host)| (id.as_str(), host))
    }

    /// Trust on first use: an unknown `host` gets `key` pinned ([`HostTrust::PinnedNow`]), a known
    /// one must show its pinned key. A different key is [`KnownHostsError::HostKeyChanged`] and
    /// changes nothing.
    pub fn check(&mut self, host: &str, key: ServerKey) -> Result<HostTrust, KnownHostsError> {
        let id = normalize_host_id(host)?;
        match self.hosts.get(&id) {
            Some(known) if known.key.matches(&key.0) => Ok(HostTrust::Known),
            Some(known) => Err(KnownHostsError::HostKeyChanged {
                host: id,
                pinned: known.key,
                offered: key,
            }),
            None => {
                if self.hosts.len() >= MAX_KNOWN_HOSTS {
                    return Err(KnownHostsError::Full);
                }
                self.hosts.insert(
                    id,
                    KnownHost {
                        key,
                        source: TrustSource::FirstUse,
                    },
                );
                Ok(HostTrust::PinnedNow)
            }
        }
    }

    /// Pins `key` for `host` by hand, replacing any earlier key; returns the earlier entry.
    pub fn trust(&mut self, host: &str, key: ServerKey) -> Result<Option<KnownHost>, KnownHostsError> {
        let id = normalize_host_id(host)?;
        if !self.hosts.contains_key(&id) && self.hosts.len() >= MAX_KNOWN_HOSTS {
            return Err(KnownHostsError::Full);
        }
        Ok(self.hosts.insert(
            id,
            KnownHost {
                key,
                source: TrustSource::Manual,
            },
        ))
    }

    /// Forgets `host`, so its next connection is a first use again; returns the entry it had.
    pub fn forget(&mut self, host: &str) -> Result<Option<KnownHost>, KnownHostsError> {
        let id = normalize_host_id(host)?;
        Ok(self.hosts.remove(&id))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn key(b: u8) -> ServerKey {
        ServerKey([b; 32])
    }

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("regia-net-known-hosts-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_first_use_pins_and_a_changed_key_is_refused() {
        let mut store = KnownHosts::new();
        assert_eq!(store.check("Game.Example:7777", key(1)), Ok(HostTrust::PinnedNow));
        assert_eq!(store.check("game.example:7777", key(1)), Ok(HostTrust::Known));
        let err = store.check(" game.example:7777 ", key(2)).expect_err("changed");
        assert_eq!(err.code(), "net_host_key_changed");
        assert_eq!(
            err,
            KnownHostsError::HostKeyChanged {
                host: "game.example:7777".into(),
                pinned: key(1),
                offered: key(2),
            }
        );
        assert_eq!(
            store.get("GAME.example:7777").map(|h| h.key),
            Some(key(1)),
            "unchanged"
        );
        assert_eq!(
            store.get("game.example:7777").map(|h| h.source),
            Some(TrustSource::FirstUse)
        );
    }

    #[test]
    fn trust_replaces_and_forget_makes_the_next_use_first_again() {
        let mut store = KnownHosts::new();
        store.check("a", key(1)).expect("pin");
        assert_eq!(
            store.trust("a", key(2)).expect("trust").map(|h| h.key),
            Some(key(1))
        );
        assert_eq!(store.check("a", key(2)), Ok(HostTrust::Known));
        assert_eq!(store.get("a").map(|h| h.source), Some(TrustSource::Manual));
        assert_eq!(store.forget("a").expect("forget").map(|h| h.key), Some(key(2)));
        assert_eq!(store.forget("a"), Ok(None));
        assert_eq!(store.check("a", key(3)), Ok(HostTrust::PinnedNow));
    }

    #[test]
    fn bad_host_ids_are_refused_with_a_code() {
        let mut store = KnownHosts::new();
        for bad in ["", "   ", "a\nb", &"x".repeat(MAX_HOST_ID_BYTES + 1)] {
            let err = store.check(bad, key(1)).expect_err("bad id");
            assert_eq!(err.code(), "net_bad_host_id", "{bad:?}");
        }
        assert!(store.is_empty());
    }

    #[test]
    fn a_full_store_refuses_new_hosts_but_keeps_checking_old_ones() {
        let mut store = KnownHosts::new();
        for i in 0..MAX_KNOWN_HOSTS {
            store.check(&format!("h{i}"), key(1)).expect("pin");
        }
        assert_eq!(
            store.check("new", key(1)).map_err(|e| e.code()),
            Err("net_known_hosts_full")
        );
        assert_eq!(
            store.trust("new", key(1)).map_err(|e| e.code()),
            Err("net_known_hosts_full")
        );
        assert_eq!(store.check("h0", key(1)), Ok(HostTrust::Known));
        assert!(store.trust("h0", key(2)).is_ok(), "replacing is not growing");
    }

    #[test]
    fn the_file_round_trips_and_damage_is_refused() {
        let dir = temp_dir("file");
        let path = dir.join("nested").join(KNOWN_HOSTS_FILE);
        assert_eq!(KnownHosts::load(&path), Ok(KnownHosts::new()), "missing is empty");
        let mut store = KnownHosts::new();
        store.check("b.example:1", key(0xB0)).expect("pin");
        store.trust("a.example:1", key(0xA0)).expect("trust");
        store.save(&path).expect("save creates the directory");
        let loaded = KnownHosts::load(&path).expect("load");
        assert_eq!(loaded, store);
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.starts_with("(\n    format_version: 1,"), "{text}");
        assert!(
            text.find("a.example").expect("a") < text.find("b.example").expect("b"),
            "sorted"
        );
        assert_eq!(KnownHosts::from_ron(&text), Ok(store.clone()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let newer = text.replace("format_version: 1", "format_version: 2");
        std::fs::write(&path, &newer).expect("write");
        assert_eq!(
            KnownHosts::load(&path).map_err(|e| e.code()),
            Err("net_known_hosts_newer")
        );
        std::fs::write(&path, "not ron").expect("write");
        assert_eq!(
            KnownHosts::load(&path).map_err(|e| e.code()),
            Err("net_known_hosts_unreadable")
        );
        let twice = "(format_version: 1, hosts: {\"A\": (key: \"0101010101010101010101010101010101010101010101010101010101010101\", source: FirstUse), \"a\": (key: \"0202020202020202020202020202020202020202020202020202020202020202\", source: FirstUse)})";
        assert_eq!(
            KnownHosts::from_ron(twice).map_err(|e| e.code()),
            Err("net_known_hosts_unreadable"),
            "one host, two keys"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sprint audit G (net2-platform) L4: on Windows the saved store gets the same owner-only
    /// DACL as `server.key`; another local account must not be granted read or write.
    #[cfg(windows)]
    #[test]
    fn a_windows_known_hosts_file_is_not_writable_by_users() {
        let dir = temp_dir("acl");
        let path = dir.join(KNOWN_HOSTS_FILE);
        let mut store = KnownHosts::new();
        store.check("a.example:1", key(0xA0)).expect("pin");
        store.save(&path).expect("save");
        let meta = std::fs::metadata(&path).expect("meta");
        assert!(
            !crate::private_file::exposed_to_other_users(&path, &meta).expect("dacl read"),
            "the saved store grants other users access"
        );
        crate::private_file::grant_well_known_for_test(
            &path,
            windows_sys::Win32::Security::WinBuiltinUsersSid,
            windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE,
        )
        .expect("grant Users write");
        let meta = std::fs::metadata(&path).expect("meta");
        assert!(
            crate::private_file::exposed_to_other_users(&path, &meta).expect("dacl read"),
            "the grant was not applied"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sprint audit G (net2-platform) L3: a destination near `MAX_PATH` still gets its store;
    /// the short temporary name beside it must not push the create over the limit.
    #[cfg(windows)]
    #[test]
    fn a_long_windows_path_still_saves_the_store() {
        let dir = temp_dir(&"a".repeat(200));
        let path = dir.join(KNOWN_HOSTS_FILE);
        assert!(
            path.as_os_str().len() >= 252,
            "the test path must be near MAX_PATH: {}",
            path.display()
        );
        let mut store = KnownHosts::new();
        store.check("a.example:1", key(0xA0)).expect("pin");
        store.save(&path).expect("save");
        assert_eq!(KnownHosts::load(&path).expect("load"), store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
