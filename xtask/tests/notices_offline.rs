// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask notices --check` on a host that cannot download crates (review fixrev4 finding 2):
//! an empty `CARGO_HOME` and `CARGO_NET_OFFLINE=true` model an offline or cold build host, where
//! no optional dependency's manifest is on disk.

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// With a fresh seal the check reads the committed table and needs nothing from the network or the
/// registry cache (ADR-5903). With a stale seal (a manifest changed and nobody ran `cargo xtask
/// notices` yet) it has to resolve, and on such a host it fails with the fix instead of a bare
/// download error.
#[test]
fn notices_check_runs_without_network_or_downloaded_manifests() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let fresh = xtask::notices::seal_is_fresh(root).expect("resolve inputs");
    let cargo_home = regia_test_utils::unique_temp_dir("notices-offline-home");
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["notices", "--check"])
        .env("CARGO_HOME", &cargo_home)
        .env("CARGO_NET_OFFLINE", "true")
        .stdin(Stdio::null())
        .output()
        .expect("run xtask");
    let _ = std::fs::remove_dir_all(&cargo_home);
    let log = text(&out);
    if fresh {
        assert!(out.status.success(), "notices --check needs no download:\n{log}");
        assert!(log.contains("notices: up to date (sealed crate table"), "{log}");
    } else {
        assert!(
            !out.status.success(),
            "a stale seal resolves, which cannot work here:\n{log}"
        );
        assert!(
            log.contains("the seal in THIRD_PARTY_NOTICES.md does not match")
                && log.contains("cargo xtask notices"),
            "the failure names the fix:\n{log}"
        );
    }
}
