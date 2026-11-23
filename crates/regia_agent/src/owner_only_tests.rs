// SPDX-License-Identifier: MPL-2.0
//! ADR-5489: the agent's owner-only files (the endpoint token, `endpoint.json`, the other secrets
//! `endpoint::write_owner_only` writes, and the safety audit log) are owner-only on Windows too. The
//! Windows tests read each file's ACL back with the system's `icacls` (a second opinion beside
//! `regia_net::private_file::exposed_to_other_users`, which checks the DACL by SID). The parser is
//! pure and tested on every host.
#![cfg(test)]

/// The ACL entries `icacls <path>` lists: the first line starts with the path, each further
/// line holds one entry, and a blank line ends the list (the summary line follows it).
fn icacls_entries(text: &str, shown: &str) -> Vec<String> {
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            break;
        }
        let entry = if index == 0 {
            line.strip_prefix(shown).unwrap_or(line)
        } else {
            line
        };
        entries.push(entry.trim().to_owned());
    }
    entries
}

/// The entries that let an account other than `user` (or the system and administrators) in:
/// `BUILTIN\Users`, `Authenticated Users`, `Everyone`, or anything inherited (`(I)`).
fn loose_entries(entries: &[String]) -> Vec<&String> {
    entries
        .iter()
        .filter(|entry| {
            let lower = entry.to_lowercase();
            lower.contains("(i)")
                || lower.contains("builtin\\users")
                || lower.contains("authenticated users")
                || lower.starts_with("everyone")
        })
        .collect()
}

#[test]
fn icacls_listing_is_split_into_entries_and_loose_ones_are_found() {
    let shown = r"C:\work\regia\t\agent\token";
    let text = format!(
        "{shown} NT AUTHORITY\\SYSTEM:(I)(F)\r\n             BUILTIN\\Users:(I)(RX)\r\n             \
         NT AUTHORITY\\Authenticated Users:(I)(M)\r\n             GAME-PC\\player:(F)\r\n\r\n\
         Successfully processed 1 files; Failed processing 0 files\r\n"
    );
    let entries = icacls_entries(&text, shown);
    assert_eq!(
        entries,
        [
            "NT AUTHORITY\\SYSTEM:(I)(F)",
            "BUILTIN\\Users:(I)(RX)",
            "NT AUTHORITY\\Authenticated Users:(I)(M)",
            "GAME-PC\\player:(F)"
        ]
    );
    assert_eq!(loose_entries(&entries).len(), 3);
    let private = [
        "NT AUTHORITY\\SYSTEM:(F)".to_owned(),
        "BUILTIN\\Administrators:(F)".to_owned(),
        "GAME-PC\\player:(F)".to_owned(),
    ];
    assert!(loose_entries(&private).is_empty());
    assert_eq!(loose_entries(&["Everyone:(R)".to_owned()]).len(), 1);
}

#[cfg(windows)]
mod windows {
    use std::path::Path;
    use std::process::Command;

    use super::{icacls_entries, loose_entries};

    fn icacls(args: &[&std::ffi::OsStr]) -> String {
        let out = Command::new("icacls").args(args).output().expect("icacls runs");
        assert!(
            out.status.success(),
            "icacls {args:?}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn entries(path: &Path) -> Vec<String> {
        icacls_entries(&icacls(&[path.as_os_str()]), path.to_string_lossy().as_ref())
    }

    /// The folder's ACL as a typical `C:\` tree hands it down (measured on the min-spec Windows laptop for
    /// ADR-5126): `BUILTIN\Users` read and `Authenticated Users` modify, inherited by new files.
    /// SIDs, so the test does not depend on the system language.
    fn loosen(dir: &Path) {
        icacls(&[
            dir.as_os_str(),
            "/grant".as_ref(),
            "*S-1-5-32-545:(OI)(CI)(RX)".as_ref(),
            "/grant".as_ref(),
            "*S-1-5-11:(OI)(CI)(M)".as_ref(),
            "/q".as_ref(),
        ]);
    }

    fn assert_owner_only(path: &Path, plain: &[String]) {
        let entries = entries(path);
        let user = std::env::var("USERNAME").expect("USERNAME").to_lowercase();
        assert!(
            loose_entries(&entries).is_empty(),
            "{}: {entries:?} (a plain file there: {plain:?})",
            path.display()
        );
        assert!(
            entries
                .iter()
                .any(|e| e.to_lowercase().contains(&user) && e.ends_with("(F)")),
            "{}: this account has full control: {entries:?}",
            path.display()
        );
        let meta = std::fs::metadata(path).expect("meta");
        assert_eq!(
            regia_net::private_file::exposed_to_other_users(path, &meta),
            Ok(false),
            "{}: the DACL grants other accounts by SID",
            path.display()
        );
    }

    /// The control: a plain file in the loosened folder takes the loose inherited entries, so the
    /// read-back would see the problem (red before ADR-5489 for every file below).
    fn plain_file_entries(dir: &Path) -> Vec<String> {
        let plain = dir.join("plain.txt");
        std::fs::write(&plain, b"x").expect("plain");
        let entries = entries(&plain);
        assert!(
            !loose_entries(&entries).is_empty(),
            "the loosened folder hands its entries down: {entries:?}"
        );
        entries
    }

    #[test]
    fn agent_token_endpoint_and_secrets_are_owner_only_on_windows() {
        let base = regia_test_utils::unique_temp_dir("agent-owner-only-win");
        std::fs::create_dir_all(&base).expect("base");
        loosen(&base);
        let plain = plain_file_entries(&base);
        let dir = base.join("agent");
        let endpoint = crate::protocol::Endpoint {
            url: "ws://127.0.0.1:1".into(),
            pid: std::process::id(),
            protocol: "0.1".into(),
            token_file: String::new(),
            app: "test".into(),
        };
        crate::endpoint::write(&dir, &endpoint, "tok").expect("written");
        crate::endpoint::write_owner_only(&dir, "mcp_http_token", b"secret").expect("secret");
        for name in ["token", "endpoint.json", "mcp_http_token"] {
            assert_owner_only(&dir.join(name), &plain);
        }
        // A second write replaces the file (rename) and stays owner-only.
        crate::endpoint::write(&dir, &endpoint, "tok2").expect("rewritten");
        assert_owner_only(&dir.join("token"), &plain);
        assert_eq!(crate::endpoint::read(&dir).expect("read").1, "tok2");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn agent_audit_log_is_owner_only_on_windows_and_an_old_loose_one_is_tightened() {
        let base = regia_test_utils::unique_temp_dir("agent-audit-owner-only-win");
        std::fs::create_dir_all(&base).expect("base");
        loosen(&base);
        let plain = plain_file_entries(&base);
        let dir = base.join("agent");

        // A new log.
        let path = dir.join("audit.jsonl");
        let mut log = crate::safety::AuditLog::open(path.clone());
        log.record(&entry("agent.hello"));
        drop(log);
        assert_owner_only(&path, &plain);

        // A log an earlier version wrote with the folder's ACL keeps its lines and is tightened.
        let old = dir.join("old").join("audit.jsonl");
        std::fs::create_dir_all(old.parent().expect("parent")).expect("old dir");
        std::fs::write(&old, b"{\"seq\":1}\n").expect("old log");
        assert!(!loose_entries(&entries(&old)).is_empty(), "the old log is loose");
        let mut log = crate::safety::AuditLog::open(old.clone());
        log.record(&entry("scene.spawn"));
        drop(log);
        assert_owner_only(&old, &plain);
        let text = std::fs::read_to_string(&old).expect("read");
        assert!(text.starts_with("{\"seq\":1}\n"), "{text}");
        assert!(text.contains("scene.spawn"), "{text}");
        let _ = std::fs::remove_dir_all(&base);
    }

    fn entry(method: &'static str) -> crate::safety::AuditEntry<'static> {
        crate::safety::AuditEntry {
            frame: 1,
            unix_ms: 1_700_000_000_000,
            method,
            params_hash: 0,
            outcome: "ok",
            error_code: None,
            duration_us: 1,
            client: "test",
        }
    }
}
