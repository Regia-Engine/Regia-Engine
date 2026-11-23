// SPDX-License-Identifier: MPL-2.0
//! The editor's dev launch options on a device (ADR-8472): the start level, the application
//! arguments and the agent transport of a launch the editor started on an Android device.
//!
//! A desktop launch gets its start level from [`crate::start_level::START_LEVEL_ENV`] and its
//! arguments on the command line. An Android app has neither: the editor's launch passes one
//! intent string extra, [`EXTRA`], with `am start --es <EXTRA> <value>`, and the runtime reads it
//! through `regia_platform::launch_intent_string_extra` when the app starts.
//!
//! The value is [`DevLaunch::encode`]'s text: `v1` and then `.`-separated fields, each a one-letter
//! tag and a value made only of `0-9` and `a-z`, so it is one plain word on the device shell's
//! command line (which `adb shell` builds by joining its arguments; ADR-7548) and needs no quoting:
//!
//! - `l<hex>`: the start level, a path under `assets/` (UTF-8 bytes as lowercase hex);
//! - `a<hex>`: one application argument (one field per argument, in order);
//! - `p<port>`: the loopback port the game's agent server binds (decimal, 1024..=65535);
//! - `t<token>`: the one-time agent token the editor made for this launch ([`check_token`]).
//!
//! `p` and `t` come together or not at all. [`DevLaunch::decode`] refuses anything else (unknown
//! tags, repeated fields, uppercase hex, a level that leaves `assets/`, a short token).
//!
//! Dev builds only: [`ENABLED`] is `cfg!(debug_assertions)`, so a shipping build (release profile)
//! never reads the extra, never takes a token from outside and never binds a fixed port.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;

/// The intent string extra that carries the encoded options (`am start --es regia.dev_launch v1...`).
pub const EXTRA: &str = "regia.dev_launch";

/// Whether this build honors a dev launch at all: debug builds (the `dev` package profile) only.
pub const ENABLED: bool = cfg!(debug_assertions);

/// The encoded value's largest accepted length in bytes.
pub const MAX_ENCODED_LEN: usize = 64 * 1024;

/// The most application arguments one launch passes (the launch profile's limit).
pub const MAX_ARGUMENTS: usize = 128;

/// The shortest accepted agent token (hex digits; the editor makes 64).
pub const MIN_TOKEN_LEN: usize = 32;

/// The longest accepted agent token.
pub const MAX_TOKEN_LEN: usize = 128;

/// The lowest port a dev launch may name for the game's agent server.
pub const MIN_PORT: u16 = 1024;

/// The agent transport of a dev launch: the device-side loopback port the game's agent server
/// binds and the token it accepts. The editor forwards a host port to `port` (`adb forward`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevAgent {
    /// The loopback port on the device.
    pub port: u16,
    /// The one-time token (lowercase hex, [`check_token`]).
    pub token: String,
}

/// The options of one dev launch (a world resource on the device when the launch carried them).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct DevLaunch {
    /// The start level, a relative path under `assets/` (`levels/town.level.ron`).
    pub start_level: Option<String>,
    /// The application arguments, already split.
    pub args: Vec<String>,
    /// The agent transport, when the editor wants to reach the game.
    pub agent: Option<DevAgent>,
}

/// Why an encoded dev launch was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevLaunchError {
    /// Longer than [`MAX_ENCODED_LEN`].
    TooLong,
    /// It does not start with `v1`.
    Version,
    /// An unknown tag, an empty field or a field given twice.
    Field(String),
    /// A value that is not lowercase hex of whole bytes, or not UTF-8.
    Hex(char),
    /// The start level is not a relative path under `assets/`.
    Level,
    /// Too many arguments, or one with a NUL.
    Arguments,
    /// The port is not a decimal in `1024..=65535`.
    Port,
    /// The token is not 32..=128 lowercase hex digits.
    Token,
    /// A port without a token or a token without a port.
    AgentIncomplete,
}

impl DevLaunchError {
    /// The stable error code (lowercase ascii, digits and `_`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooLong => "dev_launch_too_long",
            Self::Version => "dev_launch_bad_version",
            Self::Field(_) => "dev_launch_bad_field",
            Self::Hex(_) => "dev_launch_bad_hex",
            Self::Level => "dev_launch_bad_level",
            Self::Arguments => "dev_launch_bad_arguments",
            Self::Port => "dev_launch_bad_port",
            Self::Token => "dev_launch_bad_token",
            Self::AgentIncomplete => "dev_launch_agent_incomplete",
        }
    }
}

impl std::fmt::Display for DevLaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match self {
            Self::TooLong => format!("longer than {MAX_ENCODED_LEN} bytes"),
            Self::Version => "it does not start with v1".to_owned(),
            Self::Field(field) => format!("unknown, empty or repeated field {field:?}"),
            Self::Hex(c) => format!("{c:?} in a hex value (lowercase hex of UTF-8 bytes only)"),
            Self::Level => "the start level is not a relative path under assets/".to_owned(),
            Self::Arguments => format!("more than {MAX_ARGUMENTS} arguments, or one with a NUL"),
            Self::Port => format!("the agent port is not a number in {MIN_PORT}..=65535"),
            Self::Token => {
                format!("the agent token is not {MIN_TOKEN_LEN}..={MAX_TOKEN_LEN} lowercase hex digits")
            }
            Self::AgentIncomplete => "an agent port and token come together".to_owned(),
        };
        write!(f, "{}: {what}", self.code())
    }
}

impl std::error::Error for DevLaunchError {}

/// Checks an agent token a dev launch hands over: 32..=128 lowercase hex digits (the editor makes
/// 64 from two random v4 UUIDs, as the agent server's own tokens).
///
/// # Errors
///
/// [`DevLaunchError::Token`] for anything else.
pub fn check_token(token: &str) -> Result<(), DevLaunchError> {
    let ok = (MIN_TOKEN_LEN..=MAX_TOKEN_LEN).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if ok { Ok(()) } else { Err(DevLaunchError::Token) }
}

/// Checks a start level path: relative, under `assets/` (no leading `/`, no `..`, no empty part, no
/// `\` or `:`, no control characters), at most 4096 bytes (the launch profile's rule).
///
/// # Errors
///
/// [`DevLaunchError::Level`] for anything else.
pub fn check_level(level: &str) -> Result<(), DevLaunchError> {
    let bad = level.is_empty()
        || level.len() > 4096
        || level.starts_with('/')
        || level.contains(['\\', ':'])
        || level
            .split('/')
            .any(|part| part == ".." || part == "." || part.is_empty())
        || level.chars().any(char::is_control);
    if bad { Err(DevLaunchError::Level) } else { Ok(()) }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

fn unhex(text: &str) -> Result<String, DevLaunchError> {
    let digit = |c: u8| match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        other => Err(DevLaunchError::Hex(char::from(other))),
    };
    let (pairs, rest) = text.as_bytes().as_chunks::<2>();
    if let Some(&odd) = rest.first() {
        return Err(DevLaunchError::Hex(char::from(odd)));
    }
    let mut bytes = Vec::with_capacity(pairs.len());
    for [hi, lo] in pairs {
        bytes.push((digit(*hi)? << 4) | digit(*lo)?);
    }
    String::from_utf8(bytes).map_err(|_| DevLaunchError::Hex('?'))
}

impl DevLaunch {
    /// Whether the launch asks for nothing (no extra needed).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.start_level.is_none() && self.args.is_empty() && self.agent.is_none()
    }

    /// The extra's value: `v1` and the fields, only `0-9`, `a-z` and `.` (module docs).
    ///
    /// # Errors
    ///
    /// The options would not decode: a bad level, token or port, a NUL in an argument, too many
    /// arguments, or an encoding over [`MAX_ENCODED_LEN`].
    pub fn encode(&self) -> Result<String, DevLaunchError> {
        let mut out = String::from("v1");
        if let Some(level) = &self.start_level {
            check_level(level)?;
            out.push_str(".l");
            out.push_str(&hex(level.as_bytes()));
        }
        if self.args.len() > MAX_ARGUMENTS || self.args.iter().any(|a| a.contains('\0')) {
            return Err(DevLaunchError::Arguments);
        }
        for arg in &self.args {
            out.push_str(".a");
            out.push_str(&hex(arg.as_bytes()));
        }
        if let Some(agent) = &self.agent {
            if agent.port < MIN_PORT {
                return Err(DevLaunchError::Port);
            }
            check_token(&agent.token)?;
            out.push_str(&format!(".p{}.t{}", agent.port, agent.token));
        }
        if out.len() > MAX_ENCODED_LEN {
            return Err(DevLaunchError::TooLong);
        }
        Ok(out)
    }

    /// Parses an extra's value ([`Self::encode`]'s text), refusing anything else.
    ///
    /// # Errors
    ///
    /// See [`DevLaunchError`].
    pub fn decode(text: &str) -> Result<Self, DevLaunchError> {
        if text.len() > MAX_ENCODED_LEN {
            return Err(DevLaunchError::TooLong);
        }
        let mut fields = text.split('.');
        if fields.next() != Some("v1") {
            return Err(DevLaunchError::Version);
        }
        let mut out = Self::default();
        let (mut port, mut token) = (None, None);
        for field in fields {
            let mut chars = field.chars();
            let tag = chars.next().ok_or_else(|| DevLaunchError::Field(String::new()))?;
            let value = chars.as_str();
            match tag {
                'l' if out.start_level.is_none() => {
                    let level = unhex(value)?;
                    check_level(&level)?;
                    out.start_level = Some(level);
                }
                'a' => {
                    let arg = unhex(value)?;
                    if out.args.len() >= MAX_ARGUMENTS || arg.contains('\0') {
                        return Err(DevLaunchError::Arguments);
                    }
                    out.args.push(arg);
                }
                'p' if port.is_none() => {
                    let parsed = (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                        .then(|| value.parse::<u16>().ok())
                        .flatten()
                        .filter(|p| *p >= MIN_PORT)
                        .ok_or(DevLaunchError::Port)?;
                    port = Some(parsed);
                }
                't' if token.is_none() => {
                    check_token(value)?;
                    token = Some(value.to_owned());
                }
                _ => return Err(DevLaunchError::Field(field.chars().take(16).collect())),
            }
        }
        out.agent = match (port, token) {
            (Some(port), Some(token)) => Some(DevAgent { port, token }),
            (None, None) => None,
            _ => return Err(DevLaunchError::AgentIncomplete),
        };
        Ok(out)
    }
}

/// The dev launch options this process was started with: on Android (dev builds, [`ENABLED`]) the
/// launching intent's [`EXTRA`]; a value that does not decode is logged and ignored. `None` on
/// other platforms (a desktop launch uses the environment and the command line).
#[must_use]
pub fn from_launch_intent() -> Option<DevLaunch> {
    #[cfg(target_os = "android")]
    {
        if !ENABLED {
            return None;
        }
        let text = regia_platform::launch_intent_string_extra(EXTRA)?;
        decode_logged(&text)
    }
    #[cfg(not(target_os = "android"))]
    {
        None
    }
}

/// [`DevLaunch::decode`], logging (never failing) on a refused value.
#[must_use]
pub fn decode_logged(text: &str) -> Option<DevLaunch> {
    match DevLaunch::decode(text) {
        Ok(launch) => {
            tracing::info!(
                target: "regia_framework",
                start_level = launch.start_level.as_deref().unwrap_or(""),
                args = launch.args.len(),
                agent = launch.agent.is_some(),
                "dev launch options"
            );
            Some(launch)
        }
        Err(err) => {
            tracing::warn!(target: "regia_framework", "dev launch extra ignored: {err}");
            None
        }
    }
}

/// The application arguments of this run: the dev launch's on a device, else the process's command
/// line after the program name (what a standalone launch's profile passes).
#[must_use]
pub fn launch_arguments(world: &World) -> Vec<String> {
    match world.get_resource::<DevLaunch>() {
        Some(launch) => launch.args.clone(),
        None => std::env::args().skip(1).collect(),
    }
}

/// Copies a start level named relative to `assets/` out of the VFS (`assets://<level>`, the APK's
/// assets on Android) into `cache://dev_launch/<level>`, so the level loader can read it as a file.
/// Returns the host path; `None` without a VFS, a readable source or a writable cache.
#[must_use]
pub fn materialize_level(world: &World, level: &Path) -> Option<PathBuf> {
    use regia_platform::{MountPoint, Vfs, VfsPath};
    let rel = level.to_str()?.replace('\\', "/");
    check_level(&rel).ok()?;
    let vfs = world.get_resource::<Vfs>()?;
    let source = VfsPath::root(MountPoint::Assets).join(&rel).ok()?;
    let bytes = match vfs.read_sync(&source) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(target: "regia_framework", "start level {rel}: {err}");
            return None;
        }
    };
    let dest = vfs
        .host_path(&VfsPath::root(MountPoint::Cache))?
        .join("dev_launch")
        .join(&rel);
    let parent = dest.parent()?;
    if let Err(err) = std::fs::create_dir_all(parent) {
        tracing::warn!(target: "regia_framework", "start level {rel}: {}: {err}", parent.display());
        return None;
    }
    let tmp = dest.with_extension("tmp");
    let written = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &dest));
    if let Err(err) = written {
        tracing::warn!(target: "regia_framework", "start level {rel}: {}: {err}", dest.display());
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    Some(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn full() -> DevLaunch {
        DevLaunch {
            start_level: Some("levels/town square.level.ron".into()),
            args: vec![
                "--seed".into(),
                "7".into(),
                String::new(),
                "it's \"quoted\" $HOME; rm -rf / `x` ünï".into(),
            ],
            agent: Some(DevAgent {
                port: 47_811,
                token: TOKEN.into(),
            }),
        }
    }

    #[test]
    fn the_encoding_round_trips_and_is_one_shell_word() {
        let launch = full();
        let text = launch.encode().expect("encodes");
        assert!(text.starts_with("v1."), "{text}");
        assert!(
            text.bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'.'),
            "only 0-9, a-z and . reach the device shell: {text}"
        );
        assert_eq!(DevLaunch::decode(&text), Ok(launch));
        // The empty launch is just the version.
        assert_eq!(DevLaunch::default().encode().as_deref(), Ok("v1"));
        assert_eq!(DevLaunch::decode("v1"), Ok(DevLaunch::default()));
        // Arguments keep their order, empty ones included.
        let args = DevLaunch {
            args: vec!["b".into(), String::new(), "a".into()],
            ..DevLaunch::default()
        };
        assert_eq!(DevLaunch::decode(&args.encode().expect("encodes")), Ok(args));
    }

    #[test]
    fn decode_refuses_malformed_values() {
        let cases: [(&str, &str); 14] = [
            ("", "dev_launch_bad_version"),
            ("v2.l61", "dev_launch_bad_version"),
            ("v1.", "dev_launch_bad_field"),
            ("v1.x00", "dev_launch_bad_field"),
            ("v1.l61.l62", "dev_launch_bad_field"),
            ("v1.l6", "dev_launch_bad_hex"),
            ("v1.l6A", "dev_launch_bad_hex"),
            ("v1.lff", "dev_launch_bad_hex"),
            // "../x.level.ron" and "/abs.level.ron"
            ("v1.l2e2e2f782e6c6576656c2e726f6e", "dev_launch_bad_level"),
            ("v1.l2f6162732e6c6576656c2e726f6e", "dev_launch_bad_level"),
            ("v1.a00", "dev_launch_bad_arguments"),
            ("v1.p80", "dev_launch_bad_port"),
            ("v1.p70000", "dev_launch_bad_port"),
            ("v1.p47811", "dev_launch_agent_incomplete"),
        ];
        for (text, code) in cases {
            let err = DevLaunch::decode(text).expect_err(text);
            assert_eq!(err.code(), code, "{text}: {err}");
        }
        let too_long = format!("v1.a{}", "61".repeat(MAX_ENCODED_LEN));
        assert_eq!(DevLaunch::decode(&too_long), Err(DevLaunchError::TooLong));
        let many: String = std::iter::once("v1".to_owned())
            .chain((0..=MAX_ARGUMENTS).map(|_| ".a61".to_owned()))
            .collect();
        assert_eq!(DevLaunch::decode(&many), Err(DevLaunchError::Arguments));
    }

    #[test]
    fn the_token_check_takes_only_long_lowercase_hex() {
        assert_eq!(check_token(TOKEN), Ok(()));
        assert_eq!(check_token(&TOKEN[..MIN_TOKEN_LEN]), Ok(()));
        for bad in [
            "",
            &TOKEN[..MIN_TOKEN_LEN - 1],
            &TOKEN.to_uppercase(),
            &format!("{}g", &TOKEN[..40]),
            &TOKEN.repeat(3),
            &format!("{} ", &TOKEN[..40]),
        ] {
            assert_eq!(check_token(bad), Err(DevLaunchError::Token), "{bad:?}");
        }
        // A short token in the extra refuses the whole launch, and encode will not make one.
        let short = format!("v1.p47811.t{}", &TOKEN[..20]);
        assert_eq!(DevLaunch::decode(&short), Err(DevLaunchError::Token));
        let mut launch = full();
        if let Some(agent) = &mut launch.agent {
            agent.token = "secret".into();
        }
        assert_eq!(launch.encode(), Err(DevLaunchError::Token));
        let token_only = format!("v1.t{TOKEN}");
        assert_eq!(
            DevLaunch::decode(&token_only),
            Err(DevLaunchError::AgentIncomplete)
        );
    }

    #[test]
    fn a_start_level_is_copied_out_of_the_vfs_assets() {
        use regia_platform::{DirMount, MountPoint, Vfs};
        let root = regia_test_utils::unique_temp_dir("dev_launch_materialize");
        let assets = root.join("assets");
        std::fs::create_dir_all(assets.join("levels")).expect("dir");
        std::fs::write(assets.join("levels/a.level.ron"), "(level)").expect("level");
        let cache = root.join("cache");
        std::fs::create_dir_all(&cache).expect("cache");
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::Assets, DirMount::read_only(assets));
        vfs.mount(MountPoint::Cache, DirMount::writable(cache.clone()));
        let mut world = World::new();
        world.insert_resource(vfs);
        let host = materialize_level(&world, Path::new("levels/a.level.ron")).expect("copied");
        assert_eq!(host, cache.join("dev_launch/levels/a.level.ron"));
        assert_eq!(std::fs::read_to_string(&host).expect("read"), "(level)");
        assert_eq!(
            materialize_level(&world, Path::new("levels/missing.level.ron")),
            None
        );
        assert_eq!(materialize_level(&world, Path::new("../outside.level.ron")), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
