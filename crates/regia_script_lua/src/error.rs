// SPDX-License-Identifier: MPL-2.0
//! Protocol error codes (`docs/specs/84-lua-mods.md` § 4) and the classification of `mlua` errors.

use serde::{Deserialize, Serialize};

/// Why a mod call failed. The [`code`](Self::code) strings are part of the agent protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LuaErrorCode {
    /// The chunk did not compile (or was precompiled bytecode, which is refused).
    Syntax,
    /// A Lua runtime error (`error(...)`, a nil index, a bad argument).
    Runtime,
    /// The call ran past its instruction budget.
    InstructionBudget,
    /// The state hit its memory cap.
    Memory,
    /// A value crossing the boundary was not a plain value, or broke a size cap.
    BadValue,
    /// `require` named a module outside the mod's files, or one that failed to load.
    Require,
    /// The mod is disabled by an earlier error.
    Disabled,
    /// `luamod.ron` is missing, malformed, a newer version, or names a bad id or entry.
    Manifest,
    /// Reading the mod's files failed.
    Io,
    /// The mod's files are larger than the host allows.
    TooLarge,
    /// No mod has that id.
    UnknownMod,
    /// Saved mod state is corrupt or newer than this build.
    State,
    /// `regia.spawn` past the mod's entity cap (`LuaLimits::max_entities_per_mod`, or the lower
    /// cap `LuaMods::set_entity_cap` gave it; ADR-2418).
    EntityCap,
    /// Another mount already holds the id this mod's `luamod.ron` claims (or the mount itself):
    /// the mod is not loaded (ADR-7881).
    IdTaken,
}

impl LuaErrorCode {
    /// Every code, in protocol order.
    pub const ALL: [LuaErrorCode; 14] = [
        LuaErrorCode::Syntax,
        LuaErrorCode::Runtime,
        LuaErrorCode::InstructionBudget,
        LuaErrorCode::Memory,
        LuaErrorCode::BadValue,
        LuaErrorCode::Require,
        LuaErrorCode::Disabled,
        LuaErrorCode::Manifest,
        LuaErrorCode::Io,
        LuaErrorCode::TooLarge,
        LuaErrorCode::UnknownMod,
        LuaErrorCode::State,
        LuaErrorCode::EntityCap,
        LuaErrorCode::IdTaken,
    ];

    /// The protocol string (`lua_syntax`, …).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            LuaErrorCode::Syntax => "lua_syntax",
            LuaErrorCode::Runtime => "lua_runtime",
            LuaErrorCode::InstructionBudget => "lua_instruction_budget",
            LuaErrorCode::Memory => "lua_memory",
            LuaErrorCode::BadValue => "lua_bad_value",
            LuaErrorCode::Require => "lua_require",
            LuaErrorCode::Disabled => "lua_disabled",
            LuaErrorCode::Manifest => "lua_manifest",
            LuaErrorCode::Io => "lua_io",
            LuaErrorCode::TooLarge => "lua_too_large",
            LuaErrorCode::UnknownMod => "lua_unknown_mod",
            LuaErrorCode::State => "lua_state",
            LuaErrorCode::EntityCap => "lua_entity_cap",
            LuaErrorCode::IdTaken => "lua_id_taken",
        }
    }
}

impl std::fmt::Display for LuaErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// A failed mod call or load: the protocol code and a message (capped at
/// `MAX_MESSAGE_BYTES` bytes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ModError {
    /// The protocol code.
    pub code: LuaErrorCode,
    /// What went wrong.
    pub message: String,
}

/// Messages longer than this are cut (a script controls its error text).
pub(crate) const MAX_MESSAGE_BYTES: usize = 2048;

impl ModError {
    /// An error with `code` and `message` (cut to the message cap on a char boundary).
    #[must_use]
    pub fn new(code: LuaErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: cap_message(message.into()),
        }
    }
}

/// Cuts `message` to [`MAX_MESSAGE_BYTES`] on a char boundary.
pub(crate) fn cap_message(mut message: String) -> String {
    if message.len() > MAX_MESSAGE_BYTES {
        let mut end = MAX_MESSAGE_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push('…');
    }
    message
}

/// An error raised by a host function, carried through Lua as an external error so the call's
/// outcome can be classified.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub(crate) struct HostError {
    pub(crate) code: LuaErrorCode,
    pub(crate) message: String,
}

impl HostError {
    pub(crate) fn lua(code: LuaErrorCode, message: impl Into<String>) -> mlua::Error {
        mlua::Error::external(HostError {
            code,
            message: message.into(),
        })
    }
}

/// The code of an `mlua` error that left a call. Budget and memory trips are decided by the
/// caller's flags first; this classifies the rest.
pub(crate) fn classify(error: &mlua::Error) -> LuaErrorCode {
    match error {
        mlua::Error::SyntaxError { .. } => LuaErrorCode::Syntax,
        mlua::Error::MemoryError(_) => LuaErrorCode::Memory,
        mlua::Error::CallbackError { cause, .. } => classify(cause),
        mlua::Error::WithContext { cause, .. } => classify(cause),
        mlua::Error::ExternalError(inner) => match inner.downcast_ref::<HostError>() {
            Some(host) => host.code,
            None => LuaErrorCode::Runtime,
        },
        mlua::Error::BadArgument { cause, .. } => match classify(cause) {
            LuaErrorCode::Runtime => LuaErrorCode::BadValue,
            code => code,
        },
        mlua::Error::FromLuaConversionError { .. } | mlua::Error::UserDataTypeMismatch => {
            LuaErrorCode::BadValue
        }
        mlua::Error::RuntimeError(message) if is_allocator_text(message) => LuaErrorCode::Memory,
        _ => LuaErrorCode::Runtime,
    }
}

/// Whether an error's first line is Lua's out-of-memory message, bare or after the `file:line`
/// position `coroutine.wrap` adds. A script's sentence that merely holds the words is not.
fn is_allocator_text(message: &str) -> bool {
    let first = message.lines().next().unwrap_or_default();
    first == "not enough memory"
        || first
            .strip_suffix(": not enough memory")
            .and_then(|position| position.rsplit_once(':'))
            .is_some_and(|(_, line)| !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_allocators_message_reads_as_memory() {
        let code = |m: &str| classify(&mlua::Error::RuntimeError(m.to_owned()));
        assert_eq!(
            code("not enough memory\nstack traceback:\n\t[C]: ?"),
            LuaErrorCode::Memory
        );
        assert_eq!(code("mod/main.lua:3: not enough memory"), LuaErrorCode::Memory);
        assert_eq!(
            code("not enough memory while caching levels"),
            LuaErrorCode::Runtime
        );
        assert_eq!(code("cache: not enough memory"), LuaErrorCode::Runtime);
        assert_eq!(
            code("mod/main.lua:3: the cache says not enough memory left"),
            LuaErrorCode::Runtime
        );
    }

    #[test]
    fn codes_are_lowercase_protocol_identifiers() {
        for code in LuaErrorCode::ALL {
            let s = code.code();
            assert!(s.starts_with("lua_"), "{s}");
            assert!(
                s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{s}"
            );
            let json = serde_json::to_value(code).map(|v| v.as_str().map(str::to_owned));
            assert_eq!(
                json.ok().flatten().map(|n| format!("lua_{n}")),
                Some(s.to_owned())
            );
        }
    }

    #[test]
    fn long_messages_are_cut_on_a_char_boundary() {
        let long = "é".repeat(MAX_MESSAGE_BYTES);
        let error = ModError::new(LuaErrorCode::Runtime, long);
        assert!(error.message.len() <= MAX_MESSAGE_BYTES + '…'.len_utf8());
        assert!(error.message.ends_with('…'));
    }

    #[test]
    fn host_errors_keep_their_code_through_a_callback() {
        let inner = HostError::lua(LuaErrorCode::Require, "no module");
        let wrapped = mlua::Error::CallbackError {
            traceback: String::new(),
            cause: std::sync::Arc::new(inner),
        };
        assert_eq!(classify(&wrapped), LuaErrorCode::Require);
    }
}
