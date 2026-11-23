// SPDX-License-Identifier: MPL-2.0
//! Windows test helpers for owner-only files. The files themselves are created by
//! `regia_net::private_file::create_private` (ADR-5616, which replaced ADR-5126's `icacls` step):
//! a protected DACL for this process's account, `SYSTEM` and `Administrators`, set at creation.

use std::path::Path;

/// The ACL entries `icacls` lists for `path`, one per line after the path (the summary line follows
/// a blank line).
pub(crate) fn acl_entries(path: &Path) -> anyhow::Result<Vec<String>> {
    let out = std::process::Command::new("icacls").arg(path).output()?;
    anyhow::ensure!(out.status.success(), "icacls {} failed", path.display());
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let shown = path.to_string_lossy().into_owned();
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            break;
        }
        let entry = if index == 0 {
            line.strip_prefix(shown.as_str()).unwrap_or(line)
        } else {
            line
        };
        entries.push(entry.trim().to_owned());
    }
    Ok(entries)
}

/// Asserts that `path` is owner-only: no inherited entry, full control for the current account,
/// and no access-allowed entry for anyone but the file's owner, `SYSTEM` and `Administrators`
/// (read by SID, `regia_net::private_file::exposed_to_other_users`). `before` (the entries a
/// plain file had there) goes into the failure message.
pub(crate) fn assert_owner_only(path: &Path, before: &[String]) -> anyhow::Result<()> {
    let entries = acl_entries(path)?;
    let meta = std::fs::metadata(path)?;
    let exposed = regia_net::private_file::exposed_to_other_users(path, &meta).map_err(anyhow::Error::msg)?;
    assert!(
        !exposed,
        "only this account, SYSTEM and Administrators; a plain file had {before:?}, this one {entries:?}"
    );
    assert!(
        entries.iter().all(|entry| !entry.contains("(I)")),
        "no inherited entry: {entries:?}"
    );
    let user = std::env::var("USERNAME")?.to_lowercase();
    assert!(
        entries
            .iter()
            .any(|entry| entry.to_lowercase().contains(&user) && entry.ends_with("(F)")),
        "this account has full control: {entries:?}"
    );
    Ok(())
}
