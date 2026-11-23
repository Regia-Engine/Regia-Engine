// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/platform.rs, commit d6ed37677), ADR-8012.
// Adapted under the owner's 2026-11-16 exception; host-owned entropy/persistence omitted (ADR-8159).
//! Prefix classification and canonical account-id encodings. No IO or simulation state.

use std::fmt::Write as _;

use super::Store;

/// Steam account prefix.
pub const PREFIX_STEAM: &str = "steam:";
/// GOG Galaxy account prefix.
pub const PREFIX_GOG: &str = "gog:";
/// Install-local account prefix.
pub const PREFIX_LOCAL: &str = "local:";
/// Bot account prefix (classification is case-insensitive).
pub const PREFIX_BOT: &str = "bot:";

/// Prefix classification only; it does not validate or authenticate an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountKind {
    /// Steam-prefixed account.
    Steam,
    /// GOG-prefixed account.
    Gog,
    /// Install-local account.
    Local,
    /// Bot identity.
    Bot,
    /// Unprefixed hub identity, unknown prefix, or empty id.
    Hub,
}

/// Classify by prefix. Only `bot:` is case-insensitive. This is not authentication.
pub fn account_kind(id: &str) -> AccountKind {
    if id.len() >= PREFIX_BOT.len()
        && id.as_bytes()[..PREFIX_BOT.len()].eq_ignore_ascii_case(PREFIX_BOT.as_bytes())
    {
        AccountKind::Bot
    } else if id.starts_with(PREFIX_STEAM) {
        AccountKind::Steam
    } else if id.starts_with(PREFIX_GOG) {
        AccountKind::Gog
    } else if id.starts_with(PREFIX_LOCAL) {
        AccountKind::Local
    } else {
        AccountKind::Hub
    }
}

/// Store indicated by a prefix; None for local, bot, and hub ids. Not authentication.
pub fn store_of(id: &str) -> Store {
    match account_kind(id) {
        AccountKind::Steam => Store::Steam,
        AccountKind::Gog => Store::Gog,
        _ => Store::None,
    }
}

/// Format a nonzero 64-bit Galaxy id. Zero is not an account id.
pub fn account_id_from_galaxy(id: u64) -> Option<String> {
    (id != 0).then(|| format!("{PREFIX_GOG}{id}"))
}

/// Parse a canonical Galaxy id: lowercase prefix, ASCII digits, nonzero, no leading zeros.
pub fn galaxy_from_account_id(account_id: &str) -> Option<u64> {
    let digits = account_id.strip_prefix(PREFIX_GOG)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id: u64 = digits.parse().ok()?;
    (account_id_from_galaxy(id).as_deref() == Some(account_id)).then_some(id)
}

/// Format 16 caller-supplied bytes as `local:` plus 32 lowercase hexadecimal digits.
/// The host owns entropy and persistence; this function performs neither.
pub fn local_account_id(bytes: [u8; 16]) -> String {
    let mut s = String::with_capacity(PREFIX_LOCAL.len() + 32);
    s.push_str(PREFIX_LOCAL);
    for b in bytes {
        // Formatting into a String is infallible.
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Whether an id is `local:` plus exactly 32 lowercase hexadecimal digits.
pub fn is_local_account_id(id: &str) -> bool {
    id.strip_prefix(PREFIX_LOCAL)
        .is_some_and(|h| h.len() == 32 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Same-store routing hint for a store-specific relay; never an authorization check.
/// Local, bot and hub ids have no store relay. Authenticate both peers independently.
/// Cross-store transport remains the host's common transport, outside this helper.
pub fn relay_allowed(a: &str, b: &str) -> bool {
    match (store_of(a), store_of(b)) {
        (Store::None, _) | (_, Store::None) => false,
        (x, y) => x == y,
    }
}
