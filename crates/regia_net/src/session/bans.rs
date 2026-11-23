// SPDX-License-Identifier: MPL-2.0
//! The server's ban list, `bans.ron` (spec 77 § 7.3).
//!
//! Three kinds of entry: `PlatformId` (from the session authenticator), `Address` (an exact IPv4 or
//! IPv6 address, or a `/prefix` range in CIDR notation, RFC 4632; an IPv6 address without a prefix
//! bans its /64, ADR-3283) and `Name` (exact player name, the weakest kind). `until` is a Unix time in seconds, or `None` for permanent; expired entries never
//! match and are dropped on the next save.
//!
//! **Which clock.** A running server stamps and judges `until` and `added` on its session clock
//! (`NetSession::now_unix_s`: the wall second when the session was made plus the monotonic
//! session clock, ADR-4772), the clock its join check reads; the dedicated-server console, the
//! agent's `server.ban` / `server.unban` / `server.bans` and the game-port bans all use it
//! (ADR-5973). Every `now_s` argument below is that clock. A new session re-bases on the wall, so
//! the file stays in wall-clock Unix seconds across restarts, and a hand edit of `bans.ron` with
//! no session running writes wall-clock seconds.
//!
//! [`BanStore`] owns the file: every change is written atomically (temp file + rename), an
//! unreadable file at start keeps an empty in-memory list and is never overwritten.
//! `format_version: 2`; version 1 files are migrated in memory (a bare IPv6 entry becomes
//! `addr/128`, preserving its exact-address meaning, ADR-4936).

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The `format_version` this build writes.
pub const BAN_LIST_FORMAT_VERSION: u32 = 2;

/// The id of one ban entry, unique in its list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BanId(pub u64);

/// What a ban entry matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BanKind {
    /// The authenticator's platform id (`steam:...`).
    PlatformId,
    /// An IP address or a CIDR range.
    Address,
    /// An exact player name (weakest; logged as such).
    Name,
}

impl BanKind {
    /// The lowercase name used in logs and agent replies.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlatformId => "platform_id",
            Self::Address => "address",
            Self::Name => "name",
        }
    }
}

/// One entry of `bans.ron`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BanListEntry {
    /// Entry id (assigned by [`BanList::add`]).
    pub id: BanId,
    /// What `value` is.
    pub kind: BanKind,
    /// Platform id, address or CIDR range, or player name.
    pub value: String,
    /// Expiry, Unix seconds on the server's session clock (see the module docs); `None` =
    /// permanent.
    pub until: Option<u64>,
    /// Reason text shown to the banned player.
    #[serde(default)]
    pub reason: String,
    /// When it was added, Unix seconds.
    #[serde(default)]
    pub added: u64,
}

impl BanListEntry {
    /// True when the entry is still in force at `now_s`.
    pub fn active(&self, now_s: u64) -> bool {
        self.until.is_none_or(|u| u > now_s)
    }

    /// The refusal detail: reason and expiry.
    pub fn detail(&self) -> String {
        match self.until {
            Some(u) => format!("{} (until {u})", self.reason),
            None => format!("{} (permanent)", self.reason),
        }
    }
}

/// Who is joining, as the ban check sees it.
#[derive(Debug, Clone, Copy)]
pub struct BanSubject<'a> {
    /// The authenticator's platform id, if any.
    pub platform_id: Option<&'a str>,
    /// The peer's address.
    pub addr: Option<IpAddr>,
    /// The declared player names.
    pub names: &'a [String],
}

/// The ban list (spec 77 § 7.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "BanList")]
pub struct BanList {
    /// Layout version.
    pub format_version: u32,
    /// The entries.
    #[serde(default)]
    pub entries: Vec<BanListEntry>,
}

impl Default for BanList {
    fn default() -> Self {
        Self {
            format_version: BAN_LIST_FORMAT_VERSION,
            entries: Vec::new(),
        }
    }
}

/// Why the ban file could not be used.
#[derive(Debug, Error)]
pub enum BanError {
    /// Not valid RON for the ban list shape.
    #[error("bans.ron: {0}")]
    Parse(String),
    /// A `format_version` newer than this build reads.
    #[error("bans.ron: format_version {0} is newer than {BAN_LIST_FORMAT_VERSION}")]
    TooNew(u32),
    /// File IO.
    #[error("bans.ron: {0}")]
    Io(String),
    /// The file was unreadable at start; it is not overwritten until a reload reads it.
    #[error("bans.ron was unreadable at start; not overwriting it")]
    ReadOnly,
    /// An address entry that is neither an address nor a CIDR range.
    #[error("bans.ron: `{0}` is not an IP address or range")]
    BadAddress(String),
}

/// Rewrites a parsed version 1 list in place for version 2: a bare IPv6 `Address` entry gains an
/// explicit `/128`, which in v2 is the spelling of "this one address". IPv4 entries, entries that
/// already carry a prefix, and the other kinds are untouched.
fn migrate_v1_in_place(list: &mut BanList) {
    for entry in &mut list.entries {
        if entry.kind != BanKind::Address || entry.value.contains('/') {
            continue;
        }
        if let Ok(IpAddr::V6(v6)) = entry.value.parse::<IpAddr>() {
            entry.value = format!("{v6}/128");
        }
    }
    list.format_version = BAN_LIST_FORMAT_VERSION;
}

/// Migrates `bans.ron` text of an older version to the current version, returning the new text.
/// A version 1 file becomes version 2 with every bare IPv6 `Address` entry rewritten to its
/// `/128` form (its v1 meaning, ADR-4936); a current file is returned re-serialized. This is the
/// `regia upgrade-project` step for the `bans` format (spec 12 § 4a).
pub fn migrate_bans(text: &str) -> Result<String, BanError> {
    let list = BanList::from_ron(text)?;
    list.to_ron()
}

/// Parses `a.b.c.d`, an IPv6 address, or either with `/prefix`.
pub fn parse_address_range(value: &str) -> Option<(IpAddr, u8)> {
    let (addr, prefix) = match value.split_once('/') {
        Some((a, p)) => (a.trim(), Some(p.trim().parse::<u8>().ok()?)),
        None => (value.trim(), None),
    };
    let ip: IpAddr = addr.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let prefix = prefix.unwrap_or(max);
    (prefix <= max).then_some((ip, prefix))
}

fn ip_bits(ip: IpAddr) -> (u128, u8) {
    match ip {
        IpAddr::V4(v4) => (u128::from(u32::from(v4)), 32),
        IpAddr::V6(v6) => (u128::from(v6), 128),
    }
}

/// True when `ip` lies inside `range` (`/prefix` form, or one address). An IPv6 address written
/// without a prefix stands for its /64 ([`crate::addr_block`], ADR-3283): a subscriber rotates
/// addresses inside it for free, so banning one address alone is no ban. Write `/128` to ban one
/// IPv6 address only.
pub fn address_matches(range: &str, ip: IpAddr) -> bool {
    let Some((net, prefix)) = parse_address_range(range) else {
        return false;
    };
    let prefix = match net {
        IpAddr::V6(v6) if !range.contains('/') && v6.to_ipv4_mapped().is_none() => {
            crate::addr_block::IPV6_BLOCK_PREFIX
        }
        _ => prefix,
    };
    // An IPv4-mapped IPv6 peer matches IPv4 entries.
    let ip = ip.to_canonical();
    // And an IPv4-mapped entry (what a dual-stack host records for a mapped peer) is the IPv4
    // range it maps, when its prefix stays inside the mapped block (audit wave 5, net-g2c).
    let (net, prefix) = match net {
        IpAddr::V6(v6) if prefix >= 96 => v6
            .to_ipv4_mapped()
            .map_or((net, prefix), |v4| (IpAddr::V4(v4), prefix - 96)),
        _ => (net, prefix),
    };
    let (a, width) = ip_bits(ip);
    let (b, net_width) = ip_bits(net);
    if width != net_width {
        return false;
    }
    if prefix == 0 {
        return true;
    }
    let shift = u32::from(width - prefix);
    (a >> shift) == (b >> shift)
}

impl BanList {
    /// Parses `bans.ron`. Reads version 1 and 2 (and refuses newer versions). A version 1 file is
    /// migrated in memory: every bare IPv6 `Address` entry gains an explicit `/128`, so its
    /// meaning (one exact address, spec 77 § 7.3 as written for v1) is preserved instead of
    /// silently widening to a /64 (ADR-4936). The next save writes version 2.
    pub fn from_ron(text: &str) -> Result<Self, BanError> {
        let mut list: BanList = ron::from_str(text).map_err(|e| BanError::Parse(e.to_string()))?;
        if list.format_version > BAN_LIST_FORMAT_VERSION {
            return Err(BanError::TooNew(list.format_version));
        }
        if list.format_version == 1 {
            migrate_v1_in_place(&mut list);
        }
        Ok(list)
    }

    /// The file text. `\n` on every platform: ron's default is `\r\n` on Windows (ADR-4772, same
    /// fix as `KnownHosts::to_ron`, ADR-3150).
    pub fn to_ron(&self) -> Result<String, BanError> {
        let config = regia_core::ron_pretty().struct_names(true);
        ron::ser::to_string_pretty(self, config)
            .map(|mut s| {
                s.push('\n');
                s
            })
            .map_err(|e| BanError::Parse(e.to_string()))
    }

    /// The first active entry matching the subject, checked in kind order platform id, address,
    /// name.
    pub fn find(&self, who: &BanSubject<'_>, now_s: u64) -> Option<&BanListEntry> {
        let active = || self.entries.iter().filter(move |e| e.active(now_s));
        if let Some(pid) = who.platform_id
            && let Some(e) = active().find(|e| e.kind == BanKind::PlatformId && e.value == pid)
        {
            return Some(e);
        }
        if let Some(ip) = who.addr
            && let Some(e) = active().find(|e| e.kind == BanKind::Address && address_matches(&e.value, ip))
        {
            return Some(e);
        }
        if let Some(e) = active().find(|e| e.kind == BanKind::Name && who.names.contains(&e.value)) {
            tracing::info!(name = %e.value, "net session: ban matched by player name (weakest kind)");
            return Some(e);
        }
        None
    }

    /// Adds an entry; returns its new id. Address entries must parse.
    pub fn add(
        &mut self,
        kind: BanKind,
        value: &str,
        until: Option<u64>,
        reason: &str,
        now_s: u64,
    ) -> Result<BanId, BanError> {
        if kind == BanKind::Address && parse_address_range(value).is_none() {
            return Err(BanError::BadAddress(value.to_owned()));
        }
        // A retry (an admin re-runs a ban whose save failed with `BanError::NotSaved`, the entry
        // kept in memory) refreshes the existing entry instead of adding a duplicate row that
        // matches the same players (corr-low, ADR-5789).
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|e| e.kind == kind && e.value == value)
        {
            existing.until = until;
            existing.reason = reason.to_owned();
            existing.added = now_s;
            return Ok(existing.id);
        }
        let id = BanId(self.entries.iter().map(|e| e.id.0).max().unwrap_or(0) + 1);
        self.entries.push(BanListEntry {
            id,
            kind,
            value: value.to_owned(),
            until,
            reason: reason.to_owned(),
            added: now_s,
        });
        Ok(id)
    }

    /// Removes an entry; false when no entry has that id.
    pub fn remove(&mut self, id: BanId) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        self.entries.len() != before
    }

    /// Drops expired entries.
    pub fn prune(&mut self, now_s: u64) {
        self.entries.retain(|e| e.active(now_s));
    }
}

/// `bans.ron` on disk plus its in-memory list.
#[derive(Debug, Clone, Default)]
pub struct BanStore {
    path: Option<PathBuf>,
    list: BanList,
    writable: bool,
}

impl BanStore {
    /// An in-memory list with no file (tests, listen servers without a ban file).
    pub fn in_memory() -> Self {
        Self {
            path: None,
            list: BanList::default(),
            writable: true,
        }
    }

    /// Opens the file. A missing file is an empty list (created on the first change). An unreadable
    /// file logs an error, keeps an empty list, and is never overwritten until a reload reads it.
    pub fn open(path: &Path) -> Self {
        let mut store = Self {
            path: Some(path.to_owned()),
            list: BanList::default(),
            writable: true,
        };
        match std::fs::read_to_string(path) {
            Ok(text) => match BanList::from_ron(&text) {
                Ok(list) => store.list = list,
                Err(e) => {
                    tracing::error!(path = %path.display(), error = %e, "net session: ban list unreadable; keeping an empty list and not overwriting the file");
                    store.writable = false;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "net session: ban list unreadable; keeping an empty list and not overwriting the file");
                store.writable = false;
            }
        }
        store
    }

    /// Re-reads the file (SIGHUP / `reload`). On success the file becomes writable again; on
    /// failure the in-memory list is kept.
    pub fn reload(&mut self) -> Result<(), BanError> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                self.list = BanList::from_ron(&text)?;
                self.writable = true;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.writable = true;
                Ok(())
            }
            Err(e) => Err(BanError::Io(e.to_string())),
        }
    }

    /// The list.
    pub fn list(&self) -> &BanList {
        &self.list
    }

    /// The file path, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// False when the file was unreadable at start (changes stay in memory).
    pub fn writable(&self) -> bool {
        self.writable
    }

    /// Adds an entry and saves. When the save fails the entry stays in memory (it applies for
    /// this run) but the error is returned: the caller must not report a ban that `bans.ron`
    /// does not hold (a locked file on Windows, a full disk).
    pub fn add(
        &mut self,
        kind: BanKind,
        value: &str,
        until: Option<u64>,
        reason: &str,
        now_s: u64,
    ) -> Result<BanId, BanError> {
        let id = self.list.add(kind, value, until, reason, now_s)?;
        if let Err(e) = self.save(now_s) {
            tracing::warn!(error = %e, "net session: ban list kept in memory only");
            return Err(e);
        }
        Ok(id)
    }

    /// Removes an entry and saves; `Ok(false)` when the entry is unknown. When the save fails
    /// the removal stays memory-only and the error is returned.
    pub fn remove(&mut self, id: BanId, now_s: u64) -> Result<bool, BanError> {
        if !self.list.remove(id) {
            return Ok(false);
        }
        if let Err(e) = self.save(now_s) {
            tracing::warn!(error = %e, "net session: ban list kept in memory only");
            return Err(e);
        }
        Ok(true)
    }

    /// Prunes expired entries and writes the file atomically.
    pub fn save(&mut self, now_s: u64) -> Result<(), BanError> {
        let tmp = self.write_temp(now_s)?;
        let Some(path) = &self.path else {
            return Ok(());
        };
        replace_with_retry(&tmp, path)
    }

    /// The first half of [`Self::save`]: prunes and writes the temp file next to the target, without
    /// replacing the target. A crash after this leaves the old file intact. Returns the temp path
    /// (empty for an in-memory store).
    pub fn write_temp(&mut self, now_s: u64) -> Result<PathBuf, BanError> {
        if !self.writable {
            return Err(BanError::ReadOnly);
        }
        self.list.prune(now_s);
        let Some(path) = &self.path else {
            return Ok(PathBuf::new());
        };
        let text = self.list.to_ron()?;
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, text).map_err(|e| BanError::Io(e.to_string()))?;
        Ok(tmp)
    }
}

/// Replaces `path` with the temp file. On Windows the replace fails with a sharing violation
/// while another process (an editor, a scanner) holds `bans.ron` open without
/// `FILE_SHARE_DELETE`, so the replace is retried briefly before it is reported; the temp file
/// stays next to the target for the next save.
fn replace_with_retry(tmp: &Path, path: &Path) -> Result<(), BanError> {
    let mut attempts = 0u8;
    loop {
        match std::fs::rename(tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempts += 1;
                if attempts >= 3 {
                    return Err(BanError::Io(e.to_string()));
                }
                #[cfg(windows)]
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit fix-net3-determinism M3 (ADR-4772): `bans.ron` must save the same bytes on every
    /// host. ron's default newline is `\r\n` on Windows, so this only fails there; the assertion
    /// guards the contract on every platform.
    #[test]
    fn to_ron_uses_lf_on_every_platform() {
        let text = BanList::default().to_ron().expect("ron");
        assert!(text.contains('\n'), "the text has newlines");
        assert!(!text.contains('\r'), "no CR bytes on any platform");
        assert_eq!(BanList::from_ron(&text).expect("re-parse"), BanList::default());
    }

    #[test]
    fn cidr_ranges_and_exact_addresses() {
        let ip = |s: &str| s.parse::<IpAddr>().expect("ip");
        assert!(address_matches("203.0.113.7", ip("203.0.113.7")));
        assert!(!address_matches("203.0.113.7", ip("203.0.113.8")));
        assert!(address_matches("10.0.0.0/8", ip("10.200.1.1")));
        assert!(!address_matches("10.0.0.0/8", ip("11.0.0.1")));
        assert!(address_matches("2001:db8::/32", ip("2001:db8:1::5")));
        assert!(!address_matches("2001:db8::/32", ip("2001:db9::5")));
        assert!(address_matches("10.0.0.0/8", ip("::ffff:10.1.2.3")));
        // Audit wave 5 (net-g2c): an IPv4-mapped entry bans the mapped peer and the plain IPv4 one.
        assert!(address_matches("::ffff:10.1.2.3", ip("::ffff:10.1.2.3")));
        assert!(address_matches("::ffff:10.1.2.3", ip("10.1.2.3")));
        assert!(!address_matches("::ffff:10.1.2.3", ip("10.1.2.4")));
        assert!(address_matches("::ffff:10.1.0.0/112", ip("10.1.9.9")));
        assert!(!address_matches("::ffff:10.1.0.0/112", ip("10.2.0.1")));
        assert!(address_matches("0.0.0.0/0", ip("1.2.3.4")));
        assert!(parse_address_range("10.0.0.0/33").is_none());
        assert!(parse_address_range("nope").is_none());
    }

    /// Audit net1 (platform): a save failure must come back from `add` / `remove`, not print
    /// success with the change only in memory. A directory at the target makes the final rename
    /// fail on every platform (on Windows the same failure is a sharing violation while an
    /// editor holds `bans.ron` open).
    #[test]
    fn a_failed_save_is_returned_and_the_entry_stays_memory_only() {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-net-ban-save-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("bans.ron");
        std::fs::write(&path, BanList::default().to_ron().expect("ron")).expect("write");
        let mut store = BanStore::open(&path);
        assert!(store.writable());
        // The target becomes a directory: writing the temp file still works, the rename cannot.
        std::fs::remove_file(&path).expect("remove");
        std::fs::create_dir(&path).expect("dir at the target");
        let err = store
            .add(BanKind::Name, "Max", None, "test", 0)
            .expect_err("a failed save is not success");
        assert!(store.list().entries.iter().any(|e| e.value == "Max"), "{err}");
        // The temp file stays for a retry.
        assert!(dir.join("bans.ron.tmp").is_file(), "{err}");
        let id = store.list().entries[0].id;
        // The save's own error (ADR-5673: corr-net's `NotSaved` wrapper was not kept; callers map
        // every non-`BadAddress` error to `ban_store_save_failed`).
        assert!(
            matches!(err, BanError::Io(_)),
            "the rename failure comes back: {err}"
        );
        store.remove(id, 0).expect_err("removing must save too");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// fixrev2-v048c-merge-audit (ADR-5444/5673): a store that was unreadable at start does not
    /// hold a ban on disk, so `add` and `remove` must report the save failure (`ReadOnly`,
    /// answered as `ban_store_save_failed`) while the change still applies for this run. A
    /// success here makes the agent report a ban that is gone at the next restart.
    #[test]
    fn a_read_only_store_reports_the_change_as_not_saved() {
        let dir = std::env::temp_dir().join(format!(
            "regia-net-ban-ro-err-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("bans.ron");
        std::fs::write(&path, "garbage").expect("write");
        let mut store = BanStore::open(&path);
        assert!(!store.writable());
        let err = store
            .add(BanKind::Name, "Max", None, "test", 0)
            .expect_err("a ban the file does not hold is not reported as saved");
        assert!(matches!(err, BanError::ReadOnly), "{err}");
        let id = store
            .list()
            .entries
            .iter()
            .find(|e| e.value == "Max")
            .map(|e| e.id)
            .expect("the entry applies for this run");
        let err = store
            .remove(id, 0)
            .expect_err("an unban the file does not hold is not saved");
        assert!(matches!(err, BanError::ReadOnly), "{err}");
        assert!(store.list().entries.is_empty(), "the unban applies for this run");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "garbage",
            "the unreadable file is not overwritten"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    static NEXT_TEMP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// The Unix twin of the Windows locked-file case: `rename` replaces a destination another
    /// process holds open for read, so a held `bans.ron` never blocks a ban.
    #[cfg(unix)]
    #[test]
    fn rename_replaces_a_file_held_open_for_read() {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-net-ban-held-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("bans.ron");
        std::fs::write(&path, BanList::default().to_ron().expect("ron")).expect("write");
        let mut store = BanStore::open(&path);
        let held = std::fs::File::open(&path).expect("open like an editor would");
        store
            .add(BanKind::Address, "203.0.113.7", None, "held", 0)
            .expect("rename over a held file succeeds");
        drop(held);
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.contains("203.0.113.7"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows twin: `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` fails with a sharing violation
    /// while a process holds `bans.ron` without `FILE_SHARE_DELETE`; `add` must report the
    /// failure and leave the file unchanged.
    #[cfg(windows)]
    #[test]
    fn ban_rename_over_a_locked_file_is_not_success() {
        use std::os::windows::fs::OpenOptionsExt;
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-net-ban-locked-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("bans.ron");
        std::fs::write(&path, BanList::default().to_ron().expect("ron")).expect("write");
        // Like Notepad holding the file: no FILE_SHARE_DELETE.
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x0000_0001 | 0x0000_0002) // FILE_SHARE_READ | FILE_SHARE_WRITE
            .open(&path)
            .expect("hold the file");
        let mut store = BanStore::open(&path);
        store
            .add(BanKind::Name, "Max", None, "test", 0)
            .expect_err("the locked file must not report success");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(!text.contains("Max"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// corr-low: retrying a failed ban must not add a second entry. A save failure keeps the
    /// entry in memory (`BanError::NotSaved`, ADR-5580); an admin or agent that retried the same
    /// ban used to push a duplicate row with a new id, and the list matched the same players
    /// twice. The retry now refreshes the existing entry (new expiry, reason and stamp).
    #[test]
    fn retrying_a_ban_refreshes_the_entry_instead_of_duplicating_it() {
        let mut list = BanList::default();
        let first = list
            .add(BanKind::Address, "203.0.113.7", Some(1_000), "spam", 0)
            .expect("ban");
        let again = list
            .add(BanKind::Address, "203.0.113.7", Some(2_000), "spam again", 0)
            .expect("retry");
        assert_eq!(first, again, "the retry refreshes the same entry");
        assert_eq!(list.entries.len(), 1, "no duplicate row");
        assert_eq!(
            list.entries[0].until,
            Some(2_000),
            "the refresh applies the new expiry"
        );
        assert_eq!(list.entries[0].reason, "spam again");
        // A different target still gets its own entry.
        let other = list
            .add(BanKind::Address, "203.0.113.8", None, "spam", 0)
            .expect("other");
        assert_ne!(other, first);
        assert_eq!(list.entries.len(), 2);
    }

    #[test]
    fn spec_example_parses() {
        let text = r#"BanList(
    format_version: 1,
    entries: [
        (id: 1, kind: PlatformId, value: "steam:76561190000000000", until: None, reason: "cheating", added: 1793000000),
        (id: 2, kind: Address, value: "203.0.113.7", until: Some(1793600000), reason: "spam", added: 1793000000),
    ],
)"#;
        let list = BanList::from_ron(text).expect("parse");
        assert_eq!(list.entries.len(), 2);
        let back = BanList::from_ron(&list.to_ron().expect("ron")).expect("re-parse");
        assert_eq!(back, list);
    }

    /// A v1 file keeps its meaning (ADR-4936): a bare IPv6 entry stood for one exact address
    /// under v1, so loading migrates it to `/128` — never to the /64 a bare entry means in v2 —
    /// and the next save writes version 2. IPv4 entries and prefixed entries are untouched.
    #[test]
    fn v1_bare_ipv6_entries_migrate_to_exact_128() {
        let text = r#"BanList(
    format_version: 1,
    entries: [
        (id: 1, kind: Address, value: "2001:db8:1:2::dead:1", until: None, reason: "bot", added: 1793000000),
        (id: 2, kind: Address, value: "203.0.113.7", until: None, reason: "spam", added: 1793000000),
        (id: 3, kind: Address, value: "2001:db8:9::/48", until: None, reason: "range", added: 1793000000),
        (id: 4, kind: Name, value: "heckler", until: None, reason: "name", added: 1793000000),
    ],
)"#;
        let list = BanList::from_ron(text).expect("parse");
        assert_eq!(list.format_version, BAN_LIST_FORMAT_VERSION);
        assert_eq!(list.entries[0].value, "2001:db8:1:2::dead:1/128");
        assert_eq!(list.entries[1].value, "203.0.113.7", "IPv4 stays bare");
        assert_eq!(list.entries[2].value, "2001:db8:9::/48");
        assert_eq!(list.entries[3].value, "heckler");

        // The migrated entry still bans exactly the one address, not its /64.
        let ip = |s: &str| s.parse::<IpAddr>().expect("ip");
        assert!(address_matches(
            &list.entries[0].value,
            ip("2001:db8:1:2::dead:1")
        ));
        assert!(!address_matches(
            &list.entries[0].value,
            ip("2001:db8:1:2::dead:2")
        ));

        // Saving writes version 2; re-reading is stable.
        let saved = list.to_ron().expect("ron");
        assert!(saved.contains("format_version: 2"), "{saved}");
        let back = BanList::from_ron(&saved).expect("re-parse");
        assert_eq!(back, list);
    }

    /// The `upgrade-project` step: v1 text becomes v2 text with the bare IPv6 entries rewritten;
    /// current text passes through unchanged in meaning.
    #[test]
    fn migrate_bans_upgrades_v1_text() {
        let v1 = r#"BanList(
    format_version: 1,
    entries: [
        (id: 1, kind: Address, value: "2001:db8:1:2::dead:1", until: None, reason: "bot", added: 1793000000),
    ],
)"#;
        let v2 = migrate_bans(v1).expect("migrate");
        assert!(v2.contains("format_version: 2"), "{v2}");
        assert!(v2.contains("2001:db8:1:2::dead:1/128"), "{v2}");
        let again = migrate_bans(&v2).expect("idempotent");
        let list = BanList::from_ron(&again).expect("parse");
        assert_eq!(list.format_version, BAN_LIST_FORMAT_VERSION);
        assert_eq!(list.entries[0].value, "2001:db8:1:2::dead:1/128");
    }
}
