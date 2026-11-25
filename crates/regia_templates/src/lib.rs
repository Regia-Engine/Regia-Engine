// SPDX-License-Identifier: MPL-2.0
//! The project templates `regia new` and the editor's project browser write (`docs/specs/21`, `docs/specs/19` § 6).
//!
//! The template files live in this crate's `templates/` directory and are compiled in with
//! `include_str!`, so the crate builds from a packaged or vendored copy (ADR-1485): no crate
//! reads a file outside its own folder at compile time. The CLI and the editor are both Tools-layer
//! crates and cannot depend on each other, so the one copy of the file lists lives here.
//!
//! Each template's `.gitignore` is stored as `gitignore.tmpl`: `cargo vendor` drops files named
//! `.gitignore`, and the project still gets a `.gitignore` (the table maps the name).

/// The build script of every game executable (`launcher/build.rs`, the quick crate's `build.rs`,
/// the net game's `client/build.rs` and `server/build.rs`): on a Windows target it gives the exe
/// an 8 MiB main-thread stack (ADR-5181, the server since ADR-5617) and embeds the project icon as
/// a multi-size ICO and a VERSIONINFO (product name, company, file and product version) through
/// `winresource` (`docs/specs/21` § 4, ADR-2645). It has no placeholders, so
/// `regia upgrade-project` can tell an untouched copy from one the user edited.
pub const WINDOWS_BUILD_SCRIPT: &str = include_str!("../templates/windows_build.rs");

/// FNV-1a 64 hashes (LF line ends) of every earlier text of [`WINDOWS_BUILD_SCRIPT`]: before
/// ADR-5181 (no main-thread stack size) at 848a062fe, fb5dd1810 and 30624864c, e66fa95c6's
/// (before the corr-* LOW fixes of f46515d58, merged by integ-v049), and f46515d58's (before the
/// MIT-0 SPDX first line of ADR-10225).
const EARLIER_WINDOWS_BUILD_SCRIPTS: [u64; 5] = [
    0x0968_79f7_f602_ff9b,
    0xa8a8_2ea1_d5de_0b1d,
    0x3010_1d09_5091_0090,
    0xa69b_2761_f6db_aa89,
    0x2d9c_50c1_2d81_840b,
];

/// FNV-1a 64 hash (LF line ends) of the current [`WINDOWS_BUILD_SCRIPT`] (ADR-10225), pinned by
/// a test: when the script changes, move this value into `EARLIER_WINDOWS_BUILD_SCRIPTS` (so
/// projects made from today's text still plan `update`) and set it to the new text's hash.
#[cfg(test)]
const CURRENT_HASH: u64 = 0xf398_420b_d9ce_cf2f;

/// Whether `text` (CRLF or LF line ends) is an unchanged copy of an earlier engine release's
/// [`WINDOWS_BUILD_SCRIPT`]: `regia upgrade-project` replaces such a copy with the current one
/// (ADR-5181) and leaves any other build script alone.
#[must_use]
pub fn is_earlier_windows_build_script(text: &str) -> bool {
    let hash = fnv1a64(text.replace("\r\n", "\n").as_bytes());
    EARLIER_WINDOWS_BUILD_SCRIPTS.contains(&hash)
}

/// FNV-1a, 64-bit (offset basis 0xcbf29ce484222325, prime 0x100000001b3).
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The `[build-dependencies]` of [`WINDOWS_BUILD_SCRIPT`], as `(crate, TOML value)`.
pub const WINDOWS_BUILD_DEPENDENCIES: &[(&str, &str)] = &[
    ("winresource", r#""0.1.34""#),
    ("png", r#""0.18""#),
    (
        "toml",
        r#"{ version = "1", default-features = false, features = ["std", "parse"] }"#,
    ),
];

/// The comment line above the `[build-dependencies]` table of a game executable's `Cargo.toml`.
pub const WINDOWS_BUILD_DEPENDENCIES_COMMENT: &str =
    "# The Windows exe icon and version info (build.rs, ADR-2645).";

/// The `[build-dependencies]` table a game executable's `Cargo.toml` ends with (comment line,
/// header, one line per [`WINDOWS_BUILD_DEPENDENCIES`] entry, trailing newline).
#[must_use]
pub fn windows_build_dependencies_table() -> String {
    let mut out = format!("{WINDOWS_BUILD_DEPENDENCIES_COMMENT}\n[build-dependencies]\n");
    for (name, value) in WINDOWS_BUILD_DEPENDENCIES {
        out.push_str(&format!("{name} = {value}\n"));
    }
    out
}

/// Directory-root remaps every template's `.cargo/config.toml` ships (ADR-10370).
///
/// The same roots as the engine workspace (ADR-10354), on the workspace rustflag
/// targets and the other host triples that workspace names for cook determinism.
/// The creating machine's home directory is not in this text:
/// [`with_home_path_remap`] appends it last when `regia new` or the editor writes the
/// project, because a home path compiled into this crate would name the build machine.
pub const PATH_REMAP_CONFIG: &str = include_str!("../templates/path_remap.config.toml");

/// The process home directory: `HOME`, otherwise `USERPROFILE`.
///
/// Empty values are treated as unset. This is the directory [`with_home_path_remap`]
/// appends; it is read when the project is created, not when this crate is built.
#[must_use]
pub fn user_home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
}

/// `--remap-path-prefix` flag for `home`, escaped for a TOML basic string.
///
/// # Errors
///
/// The path is relative, not Unicode, or contains a character that would break the
/// flag or the TOML string (`"`, `=`, `]`, `{`, `}`, or a line break).
pub fn home_remap_flag(home: &std::path::Path) -> Result<String, String> {
    let Some(text) = home.to_str() else {
        return Err(format!(
            "home directory {} is not Unicode, so it cannot be written into .cargo/config.toml",
            home.display()
        ));
    };
    if text.is_empty() || !home.is_absolute() || text.contains(['"', '\n', '\r', '=', ']', '{', '}']) {
        return Err(format!(
            "home directory {} cannot be written into .cargo/config.toml",
            home.display()
        ));
    }
    Ok(format!(
        "--remap-path-prefix={}=/builder",
        escape_toml_basic(text)
    ))
}

/// Escape `\` for a TOML basic string. The caller has already rejected `"`.
fn escape_toml_basic(text: &str) -> String {
    text.replace('\\', "\\\\")
}

/// Append `home` as the last `--remap-path-prefix` of every `rustflags` array in `config`.
///
/// rustc applies the last matching prefix, so the home directory has to follow the
/// generic directory-root flags or the account name survives under `/builder`.
///
/// # Errors
///
/// `home` is not a path [`home_remap_flag`] accepts, or `config` has no closed
/// `rustflags` array.
pub fn with_home_path_remap(config: &str, home: &std::path::Path) -> Result<String, String> {
    let flag = home_remap_flag(home)?;
    const NEEDLE: &str = "rustflags = [";
    if !config.contains(NEEDLE) {
        return Err("cargo config has no rustflags array".to_string());
    }
    let mut out = String::with_capacity(config.len() + flag.len() * 8);
    let mut rest = config;
    let mut found = 0usize;
    while let Some(at) = rest.find(NEEDLE) {
        let after = &rest[at..];
        let Some(end) = after.find(']') else {
            return Err("cargo config has an unclosed rustflags array".to_string());
        };
        out.push_str(&rest[..at]);
        let trimmed = after[..end].trim_end();
        out.push_str(trimmed);
        if !trimmed.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("    \"");
        out.push_str(&flag);
        out.push_str("\",\n]");
        found += 1;
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    if found == 0 {
        return Err("cargo config has no rustflags array".to_string());
    }
    Ok(out)
}

/// Files of `templates/blank/`, path relative to the project root and the source text.
pub const BLANK: &[(&str, &str)] = &[
    (".cargo/config.toml", PATH_REMAP_CONFIG),
    (
        "rust-toolchain.toml",
        include_str!("../templates/rust-toolchain.toml"),
    ),
    ("Regia.toml", include_str!("../templates/blank/Regia.toml")),
    ("Cargo.toml", include_str!("../templates/blank/Cargo.toml.tmpl")),
    (".gitignore", include_str!("../templates/blank/gitignore.tmpl")),
    ("README.md", include_str!("../templates/blank/README.md")),
    (
        "game/Cargo.toml",
        include_str!("../templates/blank/game/Cargo.toml.tmpl"),
    ),
    (
        "game/src/lib.rs",
        include_str!("../templates/blank/game/src/lib.rs"),
    ),
    (
        "editor/Cargo.toml",
        include_str!("../templates/blank/editor/Cargo.toml.tmpl"),
    ),
    (
        "editor/src/main.rs",
        include_str!("../templates/blank/editor/src/main.rs"),
    ),
    (
        "launcher/Cargo.toml",
        include_str!("../templates/blank/launcher/Cargo.toml.tmpl"),
    ),
    (
        "launcher/src/main.rs",
        include_str!("../templates/blank/launcher/src/main.rs"),
    ),
    ("launcher/build.rs", WINDOWS_BUILD_SCRIPT),
    (
        "android/Cargo.toml",
        include_str!("../templates/blank/android/Cargo.toml.tmpl"),
    ),
    (
        "android/src/lib.rs",
        include_str!("../templates/blank/android/src/lib.rs"),
    ),
    (
        "assets/levels/main.level.ron",
        include_str!("../templates/blank/assets/levels/main.level.ron"),
    ),
    (
        "assets/config/weather.ron",
        include_str!("../templates/weather.ron"),
    ),
    (
        "config/.gitkeep",
        include_str!("../templates/blank/config/.gitkeep"),
    ),
    (
        "generators/.gitkeep",
        include_str!("../templates/blank/generators/.gitkeep"),
    ),
];

/// Files of `templates/first_person/`, path relative to the project root and the source text.
pub const FIRST_PERSON: &[(&str, &str)] = &[
    (".cargo/config.toml", PATH_REMAP_CONFIG),
    (
        "rust-toolchain.toml",
        include_str!("../templates/rust-toolchain.toml"),
    ),
    ("Regia.toml", include_str!("../templates/first_person/Regia.toml")),
    (
        "Cargo.toml",
        include_str!("../templates/first_person/Cargo.toml.tmpl"),
    ),
    (
        ".gitignore",
        include_str!("../templates/first_person/gitignore.tmpl"),
    ),
    ("README.md", include_str!("../templates/first_person/README.md")),
    (
        "game/Cargo.toml",
        include_str!("../templates/first_person/game/Cargo.toml.tmpl"),
    ),
    (
        "game/src/lib.rs",
        include_str!("../templates/first_person/game/src/lib.rs"),
    ),
    (
        "game/src/controls.rs",
        include_str!("../templates/first_person/game/src/controls.rs"),
    ),
    (
        "game/src/hud.rs",
        include_str!("../templates/first_person/game/src/hud.rs"),
    ),
    (
        "game/src/interact.rs",
        include_str!("../templates/first_person/game/src/interact.rs"),
    ),
    (
        "game/src/level.rs",
        include_str!("../templates/first_person/game/src/level.rs"),
    ),
    (
        "game/src/menu.rs",
        include_str!("../templates/first_person/game/src/menu.rs"),
    ),
    (
        "game/tests/agent_smoke.rs",
        include_str!("../templates/first_person/game/tests/agent_smoke.rs"),
    ),
    (
        "editor/Cargo.toml",
        include_str!("../templates/first_person/editor/Cargo.toml.tmpl"),
    ),
    (
        "editor/src/main.rs",
        include_str!("../templates/first_person/editor/src/main.rs"),
    ),
    (
        "launcher/Cargo.toml",
        include_str!("../templates/first_person/launcher/Cargo.toml.tmpl"),
    ),
    (
        "launcher/src/main.rs",
        include_str!("../templates/first_person/launcher/src/main.rs"),
    ),
    ("launcher/build.rs", WINDOWS_BUILD_SCRIPT),
    (
        "android/Cargo.toml",
        include_str!("../templates/first_person/android/Cargo.toml.tmpl"),
    ),
    (
        "android/src/lib.rs",
        include_str!("../templates/first_person/android/src/lib.rs"),
    ),
    (
        "assets/levels/main.level.ron",
        include_str!("../templates/first_person/assets/levels/main.level.ron"),
    ),
    (
        "assets/config/weather.ron",
        include_str!("../templates/weather.ron"),
    ),
    (
        "config/.gitkeep",
        include_str!("../templates/first_person/config/.gitkeep"),
    ),
    (
        "generators/.gitkeep",
        include_str!("../templates/first_person/generators/.gitkeep"),
    ),
];

/// Files of `templates/third_person/` (the M20 sample stripped to a playable starter).
pub const THIRD_PERSON: &[(&str, &str)] = &[
    (".cargo/config.toml", PATH_REMAP_CONFIG),
    (
        "rust-toolchain.toml",
        include_str!("../templates/rust-toolchain.toml"),
    ),
    ("Regia.toml", include_str!("../templates/third_person/Regia.toml")),
    (
        "Cargo.toml",
        include_str!("../templates/third_person/Cargo.toml.tmpl"),
    ),
    (
        ".gitignore",
        include_str!("../templates/third_person/gitignore.tmpl"),
    ),
    ("README.md", include_str!("../templates/third_person/README.md")),
    (
        "game/Cargo.toml",
        include_str!("../templates/third_person/game/Cargo.toml.tmpl"),
    ),
    (
        "game/src/lib.rs",
        include_str!("../templates/third_person/game/src/lib.rs"),
    ),
    (
        "game/src/character.rs",
        include_str!("../templates/third_person/game/src/character.rs"),
    ),
    (
        "game/src/collectible.rs",
        include_str!("../templates/third_person/game/src/collectible.rs"),
    ),
    (
        "game/src/hud.rs",
        include_str!("../templates/third_person/game/src/hud.rs"),
    ),
    (
        "game/src/level.rs",
        include_str!("../templates/third_person/game/src/level.rs"),
    ),
    (
        "game/src/combat.rs",
        include_str!("../templates/third_person/game/src/combat.rs"),
    ),
    (
        "game/src/enemy.rs",
        include_str!("../templates/third_person/game/src/enemy.rs"),
    ),
    (
        "game/src/menu.rs",
        include_str!("../templates/third_person/game/src/menu.rs"),
    ),
    (
        "game/src/respawn.rs",
        include_str!("../templates/third_person/game/src/respawn.rs"),
    ),
    (
        "game/src/sound.rs",
        include_str!("../templates/third_person/game/src/sound.rs"),
    ),
    (
        "game/tests/agent_smoke.rs",
        include_str!("../templates/third_person/game/tests/agent_smoke.rs"),
    ),
    (
        "editor/Cargo.toml",
        include_str!("../templates/third_person/editor/Cargo.toml.tmpl"),
    ),
    (
        "editor/src/main.rs",
        include_str!("../templates/third_person/editor/src/main.rs"),
    ),
    (
        "launcher/Cargo.toml",
        include_str!("../templates/third_person/launcher/Cargo.toml.tmpl"),
    ),
    (
        "launcher/src/main.rs",
        include_str!("../templates/third_person/launcher/src/main.rs"),
    ),
    ("launcher/build.rs", WINDOWS_BUILD_SCRIPT),
    (
        "android/Cargo.toml",
        include_str!("../templates/third_person/android/Cargo.toml.tmpl"),
    ),
    (
        "android/src/lib.rs",
        include_str!("../templates/third_person/android/src/lib.rs"),
    ),
    (
        "assets/levels/main.level.ron",
        include_str!("../templates/third_person/assets/levels/main.level.ron"),
    ),
    (
        "assets/config/weather.ron",
        include_str!("../templates/weather.ron"),
    ),
    (
        "config/.gitkeep",
        include_str!("../templates/third_person/config/.gitkeep"),
    ),
    (
        "generators/.gitkeep",
        include_str!("../templates/third_person/generators/.gitkeep"),
    ),
];

/// Files of `templates/top_down/` (spec 91): overhead camera, screen-relative movement, quicksave.
pub const TOP_DOWN: &[(&str, &str)] = &[
    (".cargo/config.toml", PATH_REMAP_CONFIG),
    (
        "rust-toolchain.toml",
        include_str!("../templates/rust-toolchain.toml"),
    ),
    ("Regia.toml", include_str!("../templates/top_down/Regia.toml")),
    (
        "Cargo.toml",
        include_str!("../templates/top_down/Cargo.toml.tmpl"),
    ),
    (".gitignore", include_str!("../templates/top_down/gitignore.tmpl")),
    ("README.md", include_str!("../templates/top_down/README.md")),
    (
        "game/Cargo.toml",
        include_str!("../templates/top_down/game/Cargo.toml.tmpl"),
    ),
    (
        "game/src/lib.rs",
        include_str!("../templates/top_down/game/src/lib.rs"),
    ),
    (
        "game/src/level.rs",
        include_str!("../templates/top_down/game/src/level.rs"),
    ),
    (
        "game/src/combat.rs",
        include_str!("../templates/top_down/game/src/combat.rs"),
    ),
    (
        "game/src/enemy.rs",
        include_str!("../templates/top_down/game/src/enemy.rs"),
    ),
    (
        "game/src/hud.rs",
        include_str!("../templates/top_down/game/src/hud.rs"),
    ),
    (
        "game/tests/agent_smoke.rs",
        include_str!("../templates/top_down/game/tests/agent_smoke.rs"),
    ),
    (
        "editor/Cargo.toml",
        include_str!("../templates/top_down/editor/Cargo.toml.tmpl"),
    ),
    (
        "editor/src/main.rs",
        include_str!("../templates/top_down/editor/src/main.rs"),
    ),
    (
        "launcher/Cargo.toml",
        include_str!("../templates/top_down/launcher/Cargo.toml.tmpl"),
    ),
    (
        "launcher/src/main.rs",
        include_str!("../templates/top_down/launcher/src/main.rs"),
    ),
    ("launcher/build.rs", WINDOWS_BUILD_SCRIPT),
    (
        "android/Cargo.toml",
        include_str!("../templates/top_down/android/Cargo.toml.tmpl"),
    ),
    (
        "android/src/lib.rs",
        include_str!("../templates/top_down/android/src/lib.rs"),
    ),
    (
        "assets/levels/main.level.ron",
        include_str!("../templates/top_down/assets/levels/main.level.ron"),
    ),
    (
        "assets/config/weather.ron",
        include_str!("../templates/weather.ron"),
    ),
    (
        "config/.gitkeep",
        include_str!("../templates/top_down/config/.gitkeep"),
    ),
    (
        "generators/.gitkeep",
        include_str!("../templates/top_down/generators/.gitkeep"),
    ),
];

/// Files of `templates/quick/`: one crate, one `src/main.rs`.
pub const QUICK: &[(&str, &str)] = &[
    (".cargo/config.toml", PATH_REMAP_CONFIG),
    (
        "rust-toolchain.toml",
        include_str!("../templates/rust-toolchain-quick.toml"),
    ),
    ("Cargo.toml", include_str!("../templates/quick/Cargo.toml.tmpl")),
    (".gitignore", include_str!("../templates/quick/gitignore.tmpl")),
    ("README.md", include_str!("../templates/quick/README.md")),
    ("src/main.rs", include_str!("../templates/quick/src/main.rs")),
    ("build.rs", WINDOWS_BUILD_SCRIPT),
];

/// Files of `templates/net_game/` (ADR-2039): a dedicated server and a client of one shared
/// simulation over encrypted UDP. The sources are `samples/net_game`'s, kept identical by that
/// sample's `template_sync` test.
pub const NET_GAME: &[(&str, &str)] = &[
    (".cargo/config.toml", PATH_REMAP_CONFIG),
    (
        "rust-toolchain.toml",
        include_str!("../templates/rust-toolchain.toml"),
    ),
    ("Regia.toml", include_str!("../templates/net_game/Regia.toml")),
    (
        "Cargo.toml",
        include_str!("../templates/net_game/Cargo.toml.tmpl"),
    ),
    (".gitignore", include_str!("../templates/net_game/gitignore.tmpl")),
    ("README.md", include_str!("../templates/net_game/README.md")),
    (
        "game/Cargo.toml",
        include_str!("../templates/net_game/game/Cargo.toml.tmpl"),
    ),
    (
        "game/src/lib.rs",
        include_str!("../templates/net_game/game/src/lib.rs"),
    ),
    (
        "game/src/lobby.rs",
        include_str!("../templates/net_game/game/src/lobby.rs"),
    ),
    (
        "game/src/view.rs",
        include_str!("../templates/net_game/game/src/view.rs"),
    ),
    (
        "game/src/hud.rs",
        include_str!("../templates/net_game/game/src/hud.rs"),
    ),
    (
        "game/src/server.rs",
        include_str!("../templates/net_game/game/src/server.rs"),
    ),
    (
        "game/tests/e2e.rs",
        include_str!("../templates/net_game/game/tests/e2e.rs"),
    ),
    (
        "game/tests/hosted.rs",
        include_str!("../templates/net_game/game/tests/hosted.rs"),
    ),
    (
        "assets/levels/arena.level.ron",
        include_str!("../templates/net_game/assets/levels/arena.level.ron"),
    ),
    (
        "server/Cargo.toml",
        include_str!("../templates/net_game/server/Cargo.toml.tmpl"),
    ),
    (
        "server/src/main.rs",
        include_str!("../templates/net_game/server/src/main.rs"),
    ),
    ("server/build.rs", WINDOWS_BUILD_SCRIPT),
    (
        "client/Cargo.toml",
        include_str!("../templates/net_game/client/Cargo.toml.tmpl"),
    ),
    (
        "client/src/main.rs",
        include_str!("../templates/net_game/client/src/main.rs"),
    ),
    ("client/build.rs", WINDOWS_BUILD_SCRIPT),
];

/// The template ids `regia new` accepts, in the order help text lists them.
pub const TEMPLATE_IDS: &[&str] = &[
    "blank",
    "first_person",
    "third_person",
    "top_down",
    "quick",
    "net_game",
];

/// The files of a template, by template id (`blank`, `first_person`, `third_person`, `top_down`, `quick`,
/// `net_game`, also spelled `net-game`):
/// each is the path relative to the project root and the source text, with `{{placeholders}}`
/// still in place.
pub fn template_files(template: &str) -> Option<&'static [(&'static str, &'static str)]> {
    match template {
        "blank" => Some(BLANK),
        "first_person" | "first-person" => Some(FIRST_PERSON),
        "third_person" | "third-person" => Some(THIRD_PERSON),
        "top_down" | "top-down" => Some(TOP_DOWN),
        "quick" => Some(QUICK),
        "net_game" | "net-game" => Some(NET_GAME),
        // Spec 61 § 2.3 names the blank template `empty`; both ids give the same files.
        "empty" => Some(BLANK),
        _ => None,
    }
}

/// Rust keywords (strict, reserved and edition 2024's `gen`) and Java's reserved words: the crate
/// name is also the last segment of the generated Android application id (ADR-5291).
const RESERVED_NAMES: &[&str] = &[
    "as",
    "async",
    "await",
    "break",
    "const",
    "continue",
    "crate",
    "else",
    "enum",
    "extern",
    "false",
    "fn",
    "for",
    "if",
    "impl",
    "in",
    "let",
    "loop",
    "match",
    "mod",
    "move",
    "mut",
    "pub",
    "ref",
    "return",
    "self",
    "static",
    "struct",
    "super",
    "trait",
    "true",
    "type",
    "unsafe",
    "use",
    "where",
    "while",
    "dyn",
    "abstract",
    "become",
    "box",
    "do",
    "final",
    "macro",
    "override",
    "priv",
    "typeof",
    "unsized",
    "virtual",
    "yield",
    "try",
    "gen",
    // Java-only reserved words.
    "assert",
    "boolean",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "default",
    "double",
    "extends",
    "finally",
    "float",
    "goto",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "short",
    "strictfp",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "void",
    "volatile",
];

/// Whether `name` (lowercase) is a Windows reserved device name: `con`, `prn`, `aux`, `nul`,
/// `com0` to `com9`, `lpt0` to `lpt9`. Windows cannot create a folder with such a name, and the crate
/// name is the project folder (spec 61 § 11).
fn is_windows_device_name(name: &str) -> bool {
    if matches!(name, "con" | "prn" | "aux" | "nul") {
        return true;
    }
    match name.as_bytes() {
        [b'c', b'o', b'm', digit] | [b'l', b'p', b't', digit] => digit.is_ascii_digit(),
        _ => false,
    }
}

/// Turns a project display name into the game's Rust package name (`"My Game"` → `my_game`), the
/// one rule `regia new` and the editor's project browser share: ASCII letters and digits are kept
/// (lowercased), runs of spaces, `-` and `_` become one `_`, everything else is dropped. `None`
/// when the name holds a quote, backslash or line break (it is written into TOML), or the result is
/// empty, starts with a digit, is a Rust or Java reserved word, or is a Windows device name.
#[must_use]
pub fn project_crate_name(name: &str) -> Option<String> {
    if name.chars().any(|c| matches!(c, '"' | '\n' | '\r' | '\\')) {
        return None;
    }
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if matches!(c, '-' | '_' | ' ') && !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_').to_string();
    let refused = out.is_empty()
        || out.starts_with(|c: char| c.is_ascii_digit())
        || RESERVED_NAMES.contains(&out.as_str())
        || is_windows_device_name(&out);
    (!refused).then_some(out)
}

/// Why [`write_project_dir`] refused or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectDirError {
    /// The target exists and is not an empty directory.
    Exists(std::path::PathBuf),
    /// A directory or file could not be written; the text names the path.
    Io(String),
}

impl std::fmt::Display for ProjectDirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists(path) => write!(f, "project directory already exists: {}", path.display()),
            Self::Io(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for ProjectDirError {}

/// Whether `root` can take a new project (spec 61 § 2.1 `location`): it does not exist, or it is an
/// empty directory (a real one, not a symlink to one).
#[must_use]
pub fn project_dir_is_free(root: &std::path::Path) -> bool {
    match std::fs::symlink_metadata(root) {
        Err(err) => err.kind() == std::io::ErrorKind::NotFound,
        Ok(meta) => {
            meta.is_dir() && std::fs::read_dir(root).is_ok_and(|mut entries| entries.next().is_none())
        }
    }
}

/// Writes a generated project into `root` (spec 61 § 2.1 `generate`), the one writer `regia new`
/// and the editor's project browser share. `root` is claimed first: created, or taken when it is
/// an empty real directory ([`project_dir_is_free`]); anything else is [`ProjectDirError::Exists`].
/// Each file is `(path relative to root, contents)`. When a write fails, what this call wrote is
/// removed, so nothing remains: a `root` it created is removed whole, an empty one it took is emptied
/// again. Files outside `root` are never touched.
///
/// # Errors
///
/// [`ProjectDirError::Exists`] for an occupied target, [`ProjectDirError::Io`] for a failed write.
pub fn write_project_dir(root: &std::path::Path, files: &[(&str, String)]) -> Result<(), ProjectDirError> {
    use std::io::ErrorKind;
    let io = |what: &str, path: &std::path::Path, err: std::io::Error| {
        ProjectDirError::Io(format!("cannot {what} {}: {err}", path.display()))
    };
    if let Some(parent) = root.parent() {
        std::fs::create_dir_all(parent).map_err(|err| io("create", parent, err))?;
    }
    let took_empty = match std::fs::create_dir(root) {
        Ok(()) => false,
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            if !project_dir_is_free(root) {
                return Err(ProjectDirError::Exists(root.to_path_buf()));
            }
            true
        }
        Err(err) => return Err(io("create", root, err)),
    };
    let written = (|| {
        for (rel, body) in files {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|err| io("create", parent, err))?;
            }
            std::fs::write(&path, body).map_err(|err| io("write", &path, err))?;
        }
        Ok(())
    })();
    let Err(err) = written else {
        return Ok(());
    };
    let cleanup = if took_empty {
        empty_dir(root)
    } else {
        std::fs::remove_dir_all(root)
    };
    match cleanup {
        Ok(()) => Err(err),
        Err(cleanup) => Err(ProjectDirError::Io(format!(
            "{err}; cannot remove partial project {}: {cleanup}",
            root.display()
        ))),
    }
}

/// Removes everything inside `dir` (symlinks themselves, never their targets) and keeps `dir`.
fn empty_dir(dir: &std::path::Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if std::fs::symlink_metadata(&path)?.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// The game executables' Windows build script, compiled here so its helpers are tested
/// (`main` and the `winresource` call are left out of test builds).
#[cfg(test)]
#[path = "../templates/windows_build.rs"]
mod windows_build;

#[cfg(test)]
mod windows_build_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    /// Small portability oracle for the path rules exercised by the shipped project templates.
    fn portability_problem(paths: &[&str]) -> Option<String> {
        let mut seen = HashMap::<String, &str>::new();
        for path in paths {
            let has_drive_prefix = path.as_bytes().get(1) == Some(&b':');
            if path.is_empty() || path.starts_with('/') || path.starts_with("\\\\") || has_drive_prefix {
                return Some(format!("absolute or empty path: {path}"));
            }
            if path.contains('\\') {
                return Some(format!("non-portable separator: {path}"));
            }
            let mut normalized = Vec::new();
            for component in path.split('/') {
                if component.is_empty() || component == "." {
                    return Some(format!("empty or current component: {path}"));
                }
                if component == ".." {
                    return Some(format!("parent component: {path}"));
                }
                if component.ends_with(['.', ' ']) {
                    return Some(format!("trailing dot or space: {path}"));
                }
                let basename = component.split('.').next().unwrap_or_default();
                let upper = basename.to_ascii_uppercase();
                let numbered_device = ["COM", "LPT"].iter().any(|prefix| {
                    upper.strip_prefix(prefix).is_some_and(|suffix| {
                        suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
                    })
                });
                if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") || numbered_device {
                    return Some(format!("reserved device basename: {path}"));
                }
                normalized.push(component.to_ascii_lowercase());
            }
            let key = normalized.join("/");
            if let Some(previous) = seen.insert(key, path) {
                return Some(format!("case-insensitive collision: {previous} and {path}"));
            }
        }
        None
    }

    fn embedded_path_set<'a>(files: &'a [(&'a str, &str)]) -> HashSet<&'a str> {
        files.iter().map(|(path, _)| *path).collect()
    }

    fn resolve_relative_path(base: &str, relative: &str, files: &HashSet<&str>) -> bool {
        let mut parts: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
        for part in relative.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    if parts.pop().is_none() {
                        return false;
                    }
                }
                value => parts.push(value),
            }
        }
        files.contains(parts.join("/").as_str())
    }

    fn resolve_relative_manifest(base: &str, relative: &str, files: &HashSet<&str>) -> bool {
        let mut parts: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
        for part in relative.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    if parts.pop().is_none() {
                        return false;
                    }
                }
                value => parts.push(value),
            }
        }
        files.contains(format!("{}/Cargo.toml", parts.join("/")).as_str())
    }

    #[test]
    fn every_desktop_smoke_can_exit_and_manifest_uses_the_engine_line() {
        for &id in TEMPLATE_IDS {
            let files = template_files(id).unwrap_or(&[]);
            for (path, text) in files {
                if *path == "Regia.toml" {
                    assert!(text.contains("{{engine_line}}"), "{id}: stale engine line");
                }
                // `client/src/main.rs`: the net_game client, the windowed exe of that template; its
                // own argument parser refused the flag (Windows native pass, ADR-5489 lane).
                if matches!(
                    *path,
                    "launcher/src/main.rs" | "src/main.rs" | "client/src/main.rs"
                ) {
                    assert!(
                        text.contains("--exit-after-frames"),
                        "{id}/{path}: smoke cannot terminate"
                    );
                }
            }
        }
    }

    #[test]
    fn every_template_has_a_manifest_and_unique_paths() {
        for id in TEMPLATE_IDS.iter().copied() {
            let files = template_files(id).unwrap_or(&[]);
            assert!(!files.is_empty(), "{id} has files");
            let mut paths: Vec<&str> = files.iter().map(|(p, _)| *p).collect();
            assert!(paths.contains(&"Cargo.toml"), "{id} has a Cargo.toml");
            paths.sort_unstable();
            let before = paths.len();
            paths.dedup();
            assert_eq!(before, paths.len(), "{id} lists a path twice");
        }
        assert!(template_files("nope").is_none());
        for (hyphen, snake) in [
            ("first-person", "first_person"),
            ("third-person", "third_person"),
            ("top-down", "top_down"),
            ("net-game", "net_game"),
        ] {
            assert_eq!(template_files(hyphen), template_files(snake), "{hyphen}");
            assert!(template_files(hyphen).is_some(), "{hyphen}");
        }
        let match_arms = [
            "blank",
            "first_person",
            "third_person",
            "top_down",
            "quick",
            "net_game",
        ];
        assert_eq!(TEMPLATE_IDS, match_arms, "TEMPLATE_IDS lists every template");
    }

    /// Specs/21 § 4 common step 4 (ADR-5627): `regia package --profile shipping` runs
    /// `cargo build --profile shipping`, so every generated project defines that profile, with the
    /// engine workspace's settings (fat LTO, one codegen unit, abort, line tables, no cargo strip).
    /// Before the fix no template defined it and a fresh project's shipping package failed in cargo.
    #[test]
    fn every_template_defines_the_engine_shipping_profile() {
        fn shipping(manifest: &str, what: &str) -> toml::Table {
            let table: toml::Table = toml::from_str(manifest).unwrap_or_else(|e| panic!("{what}: {e}"));
            table
                .get("profile")
                .and_then(|p| p.get("shipping"))
                .and_then(toml::Value::as_table)
                .cloned()
                .unwrap_or_else(|| panic!("{what} has no [profile.shipping]"))
        }
        let engine = shipping(include_str!("../../../Cargo.toml"), "the engine workspace");
        assert_eq!(engine.get("strip").and_then(toml::Value::as_str), Some("none"));
        for id in TEMPLATE_IDS.iter().copied() {
            let files = template_files(id).unwrap_or(&[]);
            let root = files
                .iter()
                .find(|(path, _)| *path == "Cargo.toml")
                .map(|(_, text)| *text)
                .unwrap_or_else(|| panic!("{id} has a root Cargo.toml"));
            assert_eq!(
                shipping(root, id),
                engine,
                "{id}: [profile.shipping] differs from the engine's"
            );
        }
    }

    #[test]
    fn portability_oracle_accepts_template_paths_and_separate_same_basenames() {
        for &id in TEMPLATE_IDS {
            let files = template_files(id).unwrap_or(&[]);
            let paths: Vec<&str> = files.iter().map(|(path, _)| *path).collect();
            assert_eq!(portability_problem(&paths), None, "{id}");
        }
        assert_eq!(portability_problem(&["src/lib.rs", "tests/lib.rs"]), None);
        assert_eq!(portability_problem(&[".gitignore", "config/.gitkeep"]), None);
    }

    #[test]
    fn portability_oracle_rejects_absolute_parent_alias_collision_and_devices() {
        for (paths, expected) in [
            (&["/root/file"][..], "absolute"),
            (&["C:/root/file"][..], "absolute"),
            (&["../outside"][..], "parent"),
            (&["dir/../outside"][..], "parent"),
            (&["name."][..], "trailing"),
            (&["name "][..], "trailing"),
            (&["Data/Map.ron", "data/map.ron"][..], "collision"),
            (&["CON.txt"][..], "reserved"),
            (&["folder/Lpt9.data"][..], "reserved"),
        ] {
            let problem = portability_problem(paths).unwrap_or_default();
            assert!(problem.contains(expected), "{paths:?}: {problem}");
        }
    }

    #[test]
    fn embedded_workspace_members_and_local_manifest_paths_resolve() {
        for &id in TEMPLATE_IDS {
            let files = template_files(id).unwrap_or(&[]);
            let file_set = embedded_path_set(files);
            let root_manifest = files
                .iter()
                .find(|(path, _)| *path == "Cargo.toml")
                .map(|(_, text)| *text);
            if let Some(text) = root_manifest {
                for line in text.lines().filter(|line| {
                    let line = line.trim_start();
                    line.starts_with("members = [") || line.starts_with("default-members = [")
                }) {
                    let members = line
                        .split('[')
                        .nth(1)
                        .unwrap_or_default()
                        .split(']')
                        .next()
                        .unwrap_or_default();
                    for member in members.split(',').filter_map(|item| item.split('"').nth(1)) {
                        assert!(
                            resolve_relative_manifest("", member, &file_set),
                            "{id} workspace member {member} has no embedded Cargo.toml"
                        );
                    }
                }
            }
            for (manifest, text) in files.iter().filter(|(path, _)| path.ends_with("Cargo.toml")) {
                let base = manifest.rsplit_once('/').map_or("", |(parent, _)| parent);
                for line in text.lines() {
                    let Some((_, value)) = line.split_once("path = ") else {
                        continue;
                    };
                    let Some(relative) = value.split('"').nth(1) else {
                        continue;
                    };
                    if relative.starts_with("{{engine}}") {
                        continue;
                    }
                    if line.trim_start().starts_with("path = ") {
                        assert!(
                            resolve_relative_path(base, relative, &file_set),
                            "{id}/{manifest} local source path {relative} is not embedded"
                        );
                        continue;
                    }
                    assert!(
                        resolve_relative_manifest(base, relative, &file_set),
                        "{id}/{manifest} local path {relative} has no embedded Cargo.toml"
                    );
                }
            }
        }
    }

    /// The Windows GUI subsystem (ADR-2306): every game executable's main asks for it in release
    /// builds, so a shipped game opens no console window; the dedicated server and the editor
    /// anchor stay console programs.
    #[test]
    fn game_mains_ask_for_the_windows_gui_subsystem_and_consoles_do_not() {
        const ATTRIBUTE: &str =
            r#"#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]"#;
        let mut games = 0;
        for &id in TEMPLATE_IDS {
            for (path, text) in template_files(id).unwrap_or(&[]) {
                if !path.ends_with("main.rs") {
                    continue;
                }
                let game = matches!(
                    *path,
                    "launcher/src/main.rs" | "src/main.rs" | "client/src/main.rs"
                );
                assert_eq!(text.contains(ATTRIBUTE), game, "{id}/{path}");
                if game {
                    // An inner attribute must come before the first item.
                    let at = text.find(ATTRIBUTE).unwrap_or(usize::MAX);
                    let first_item = ["\nfn ", "\nuse ", "\nmod "]
                        .iter()
                        .filter_map(|k| text.find(k))
                        .min()
                        .unwrap_or(usize::MAX);
                    assert!(at < first_item, "{id}/{path}: the attribute comes after an item");
                    games += 1;
                }
            }
        }
        assert_eq!(games, 6, "four launchers, the quick app and the net client");
    }

    /// Every game executable carries the Windows build script and its build-dependencies
    /// (ADR-2645), and so does the net game's dedicated server (its main-thread stack, ADR-5617);
    /// the editor and the Android library do not.
    #[test]
    fn game_executables_have_the_windows_build_script_and_its_dependencies() {
        assert!(
            !WINDOWS_BUILD_SCRIPT.contains("{{"),
            "the build script has no placeholders, so upgrade-project can compare it"
        );
        let table = windows_build_dependencies_table();
        assert!(table.starts_with(WINDOWS_BUILD_DEPENDENCIES_COMMENT));
        let mut scripts = 0;
        for &id in TEMPLATE_IDS {
            let files = template_files(id).unwrap_or(&[]);
            for (path, text) in files {
                let Some(dir) = path.strip_suffix("build.rs") else {
                    continue;
                };
                assert_eq!(*text, WINDOWS_BUILD_SCRIPT, "{id}/{path}");
                assert!(
                    matches!(dir, "launcher/" | "" | "client/" | "server/"),
                    "{id}/{path}: only game executables and the dedicated server"
                );
                let manifest = format!("{dir}Cargo.toml");
                let cargo = files.iter().find(|(p, _)| *p == manifest).map_or("", |(_, t)| *t);
                assert!(
                    cargo.contains(&table),
                    "{id}/{manifest} has the build-dependencies"
                );
                assert!(
                    files.iter().any(|(p, _)| *p == format!("{dir}src/main.rs")),
                    "{id}/{path} sits beside a main.rs"
                );
                scripts += 1;
            }
        }
        assert_eq!(
            scripts, 7,
            "four launchers, the quick app, the net client and the net server"
        );
    }

    /// The channel line of a `rust-toolchain.toml`.
    fn channel(text: &str) -> Option<&str> {
        text.lines()
            .find_map(|l| l.trim().strip_prefix("channel"))
            .and_then(|rest| rest.split('"').nth(1))
    }

    #[test]
    fn every_template_pins_the_engines_toolchain() {
        // The engine checkout's own pin (absent in a packaged copy: then only the shape is checked).
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rust-toolchain.toml");
        let engine = std::fs::read_to_string(root).ok();
        for &id in TEMPLATE_IDS {
            let files = template_files(id).unwrap_or(&[]);
            let Some((_, text)) = files.iter().find(|(p, _)| *p == "rust-toolchain.toml") else {
                panic!("{id} has no rust-toolchain.toml");
            };
            let pinned = channel(text).unwrap_or_default();
            assert!(
                !pinned.is_empty() && pinned != "stable",
                "{id}: a concrete channel, got `{pinned}`"
            );
            assert!(
                text.contains("clippy") && text.contains("rustfmt"),
                "{id}: components"
            );
            if let Some(engine) = &engine {
                assert_eq!(channel(engine), Some(pinned), "{id} pins the engine's compiler");
            }
            let android = text.contains("aarch64-linux-android");
            assert_eq!(
                android,
                id != "quick",
                "{id}: android targets only where there is an android crate"
            );
        }
    }

    /// The two character templates wire the save flow (spec 91 § 2.4).
    #[test]
    fn character_templates_quicksave_on_f5_and_f9() {
        for id in ["third_person", "top_down"] {
            let files = template_files(id).unwrap_or(&[]);
            let lib = files
                .iter()
                .find(|(path, _)| *path == "game/src/lib.rs")
                .map_or("", |(_, text)| *text);
            for needle in [
                "SaveFlowPlugin",
                "\"QuickSave\"",
                "\"F5\"",
                "\"F9\"",
                "PlayerCheckpoint",
            ] {
                assert!(lib.contains(needle), "{id} game lib names {needle}");
            }
        }
        let top_down = template_files("top_down").unwrap_or(&[]);
        let lib = top_down
            .iter()
            .find(|(path, _)| *path == "game/src/lib.rs")
            .map_or("", |(_, text)| *text);
        assert!(
            lib.contains("CameraMode::Overhead"),
            "top_down uses the overhead camera"
        );
        assert!(lib.contains("camera_relative_move"), "screen-relative movement");
    }

    /// The Third Person template carries the sample's features and is self-contained (ADR-2559):
    /// no `TODO(template)` stub is left, every source file is in the list, nothing reaches into
    /// `samples/`, and the generated crate ships its agent-protocol smoke tests.
    #[test]
    fn third_person_template_is_filled_in_and_self_contained() {
        let files = template_files("third_person").unwrap_or(&[]);
        let text = |path: &str| files.iter().find(|(p, _)| *p == path).map_or("", |(_, t)| *t);
        for path in [
            "game/src/lib.rs",
            "game/src/level.rs",
            "game/src/character.rs",
            "game/src/collectible.rs",
            "game/src/hud.rs",
            "game/tests/agent_smoke.rs",
        ] {
            assert!(!text(path).is_empty(), "third_person lists {path}");
        }
        for (path, body) in files {
            assert!(
                !body.contains("TODO(template)"),
                "third_person/{path}: a stub marker is left"
            );
            assert!(
                !body.contains("samples/third_person/") && !body.contains("../samples"),
                "third_person/{path}: reaches into the engine's samples"
            );
        }
        // The pieces the README names.
        let lib = text("game/src/lib.rs");
        for needle in ["SaveFlowPlugin", "CollectProgress", "\"Pause\"", "pub mod hud;"] {
            assert!(lib.contains(needle), "game lib names {needle}");
        }
        assert!(text("game/src/collectible.rs").contains("fn collect_nearby"));
        assert!(text("game/src/character.rs").contains("fn animate_rig"));
        assert!(text("game/src/hud.rs").contains("struct HudLabel"));
        assert!(text("game/src/hud.rs").contains("SaveRequest::Save"));
        let readme = text("README.md");
        for part in [
            "level.rs",
            "character.rs",
            "collectible.rs",
            "hud.rs",
            "agent_smoke.rs",
        ] {
            assert!(readme.contains(part), "the README walks through {part}");
        }
        // The smoke tests drive the game through the agent protocol and name the HUD counter.
        let smoke = text("game/tests/agent_smoke.rs");
        for needle in [
            "fn headless_smoke_runs_a_fixed_number_of_frames",
            "fn agent_scripted_input_moves_the_character",
            "fn agent_collecting_a_coin_updates_the_hud",
            "play.inject_input",
            "play.wait_until",
            "hud-counter",
            "save.load",
        ] {
            assert!(smoke.contains(needle), "agent_smoke.rs names {needle}");
        }
        assert!(
            text("game/Cargo.toml").contains("serde_json"),
            "the smoke tests' dev-dependency"
        );
        // Every `{{placeholder}}` is one the generator fills.
        for (path, body) in files {
            let mut rest = *body;
            while let Some(at) = rest.find("{{") {
                let tail = &rest[at + 2..];
                let end = tail.find("}}").unwrap_or(0);
                let key = &tail[..end];
                assert!(
                    ["name", "crate_name", "engine", "engine_line", "id"].contains(&key),
                    "third_person/{path}: unknown placeholder `{key}`"
                );
                rest = &tail[end..];
            }
        }
    }

    /// The three character templates (ADR-2738): no stub markers, nothing reaching into the
    /// engine's `samples/`, a README that says where to extend, an agent smoke test that talks
    /// to the game through `regia_agent`, no `unwrap()`/`expect()` in the game code (game developers
    /// copy it), and every template under the 1 MB size budget of spec 61 § 2.3.
    #[test]
    fn character_templates_are_complete_small_and_panic_free() {
        for id in ["third_person", "first_person", "top_down"] {
            let files = template_files(id).unwrap_or(&[]);
            let text = |path: &str| files.iter().find(|(p, _)| *p == path).map_or("", |(_, t)| *t);
            for (path, body) in files {
                assert!(
                    !body.contains("TODO(template)"),
                    "{id}/{path}: a stub marker is left"
                );
                // Prose may point a reader at the engine's samples; code and manifests may not.
                assert!(
                    path.ends_with(".md") || (!body.contains("samples/") && !body.contains("../samples")),
                    "{id}/{path}: reaches into the engine's samples"
                );
                if path.starts_with("game/src/") {
                    // Code before the unit tests only: tests may `expect`.
                    let code = body.split("#[cfg(test)]").next().unwrap_or(body);
                    for needle in [".unwrap()", ".expect("] {
                        assert!(!code.contains(needle), "{id}/{path}: {needle} in game code");
                    }
                }
            }
            let readme = text("README.md");
            assert!(
                readme.contains("## Where to extend"),
                "{id}: README has a where-to-extend section"
            );
            let smoke = text("game/tests/agent_smoke.rs");
            for needle in [
                "play.inject_input",
                "play.wait_until",
                "save.save",
                "save.load",
                "fn headless_smoke_runs_a_fixed_number_of_frames",
            ] {
                assert!(smoke.contains(needle), "{id}: agent_smoke.rs names {needle}");
            }
            assert!(
                text("game/Cargo.toml").contains("serde_json"),
                "{id}: the smoke's dev-dependency"
            );
        }
    }

    /// Every template stays under 1 MB of text (spec 61 § 2.3: the templates are compiled into the
    /// `regia` binary and carry no binary assets).
    #[test]
    fn every_template_is_under_one_megabyte() {
        for id in TEMPLATE_IDS.iter().copied() {
            let bytes: usize = template_files(id)
                .unwrap_or(&[])
                .iter()
                .map(|(_, body)| body.len())
                .sum();
            assert!(bytes < 1_000_000, "{id} is {bytes} bytes");
        }
    }

    /// The first-person template has the interaction, crosshair, pause and settings, and save the
    /// spec asks for; the top-down one has click movement, a navmesh enemy with a behavior tree
    /// and a health bar.
    #[test]
    fn first_person_and_top_down_carry_the_spec_features() {
        let fp = template_files("first_person").unwrap_or(&[]);
        let fp_text = |path: &str| fp.iter().find(|(p, _)| *p == path).map_or("", |(_, t)| *t);
        assert!(fp_text("game/src/interact.rs").contains("fn use_focus"));
        assert!(fp_text("game/src/hud.rs").contains("struct Crosshair"));
        assert!(fp_text("game/src/hud.rs").contains("PauseAction::Save"));
        assert!(fp_text("game/src/menu.rs").contains("MenuAction::Master"));
        assert!(fp_text("game/src/lib.rs").contains("SaveFlowPlugin"));
        let td = template_files("top_down").unwrap_or(&[]);
        let td_text = |path: &str| td.iter().find(|(p, _)| *p == path).map_or("", |(_, t)| *t);
        assert!(td_text("game/src/enemy.rs").contains("NavCrowd"));
        assert!(td_text("game/src/enemy.rs").contains("BehaviorTree"));
        assert!(td_text("game/src/enemy.rs").contains("HealthBar"));
        assert!(td_text("game/src/lib.rs").contains("fn pick_move_target"));
        assert!(
            td.iter().any(|(path, _)| *path == "game/src/level.rs"),
            "top_down ships game/src/level.rs"
        );
        assert!(
            td.iter()
                .all(|(path, _)| *path != "game/src/state.rs" && *path != "game/src/character.rs"),
            "top_down is not the third-person file list"
        );
        let tp = template_files("third_person").unwrap_or(&[]);
        let tp_text = |path: &str| tp.iter().find(|(p, _)| *p == path).map_or("", |(_, t)| *t);
        for (path, needle) in [
            ("game/src/combat.rs", "fn player_attack"),
            ("game/src/enemy.rs", "fn think"),
            ("game/src/respawn.rs", "fn run_respawn"),
            ("game/src/menu.rs", "MenuAction::OpenSettings"),
            ("game/src/sound.rs", "enum SoundCue"),
        ] {
            assert!(tp_text(path).contains(needle), "third_person/{path} has {needle}");
        }
    }

    /// The net-game template has a dedicated server, a client and the shared game crate, and its
    /// game runs the end-to-end test (ADR-2039).
    #[test]
    fn net_game_template_has_server_client_and_the_end_to_end_test() {
        let files = template_files("net-game").unwrap_or(&[]);
        assert_eq!(template_files("net_game").map(<[_]>::len), Some(files.len()));
        let text = |path: &str| files.iter().find(|(p, _)| *p == path).map_or("", |(_, t)| *t);
        assert!(text("Cargo.toml").contains(r#"members = ["game", "server", "client"]"#));
        assert!(text("server/Cargo.toml").contains("default-features = false"));
        assert!(text("server/Cargo.toml").contains(r#"features = ["server"]"#));
        assert!(text("Cargo.toml").contains(r#"default-members = ["game", "client"]"#));
        assert!(text("server/src/main.rs").contains("Side::Server"));
        assert!(text("client/src/main.rs").contains("Side::Client"));
        for needle in [
            "CryptoMode::Required",
            "LagCompPlugin",
            "predict::<Transform>",
            "LinkInterpolationPlugin",
        ] {
            assert!(
                text("game/src/lib.rs").contains(needle),
                "game lib names {needle}"
            );
        }
        assert!(text("game/tests/e2e.rs").contains("fn a_scripted_minute_over_encrypted_udp_agrees"));
    }

    /// Audit v033b (Med): the game crate depended on `regia` with only `net` (so `cargo check -p
    /// <game>` hit `regia`'s `compile_error`), and its `server` dev-dependency joined the client's
    /// build in a plain `cargo test`, making the client's `run_with` the dedicated-server host.
    /// The game builds with `client` by default, the server crate turns that default off, and no
    /// dev-dependency enables `server`.
    #[test]
    fn net_game_crates_always_build_one_regia_mode() {
        let files = template_files("net-game").unwrap_or(&[]);
        let text = |path: &str| files.iter().find(|(p, _)| *p == path).map_or("", |(_, t)| *t);
        let game = text("game/Cargo.toml");
        assert!(game.contains(r#"default = ["client"]"#), "{game}");
        assert!(
            !game.contains("[dev-dependencies]"),
            "no dev-dependency picks a regia mode: {game}"
        );
        let server = text("server/Cargo.toml");
        assert!(
            server.contains(
                r#"{{crate_name}} = { path = "../game", default-features = false, features = ["server"] }"#
            ),
            "the server build never gets the game's client default: {server}"
        );
        let client = text("client/Cargo.toml");
        assert!(!client.contains("server"), "{client}");
    }

    /// ADR-5181 / ADR-5617: the dedicated server is an exe too, so it carries the same Windows
    /// build script (the 8 MiB main-thread stack) and its build-dependencies; `cargo run -p
    /// <game>_server` in debug on Windows starts with the same headroom as the client.
    #[test]
    fn net_game_server_carries_the_windows_build_script() {
        let files = template_files("net_game").unwrap_or(&[]);
        let text = |path: &str| files.iter().find(|(p, _)| *p == path).map(|(_, t)| *t);
        assert_eq!(
            text("server/build.rs"),
            Some(WINDOWS_BUILD_SCRIPT),
            "server/build.rs is the template's Windows build script"
        );
        let server = text("server/Cargo.toml").unwrap_or_default();
        assert!(
            server.contains(&windows_build_dependencies_table()),
            "server/Cargo.toml has the build-dependencies: {server}"
        );
    }

    /// The current [`WINDOWS_BUILD_SCRIPT`]'s hash is pinned, so an edit to the script cannot
    /// leave projects made from this text unrecognised by `regia upgrade-project` (ADR-5181).
    #[test]
    fn the_current_windows_build_script_hash_is_pinned() {
        assert_eq!(
            fnv1a64(WINDOWS_BUILD_SCRIPT.replace("\r\n", "\n").as_bytes()),
            CURRENT_HASH,
            "templates/windows_build.rs changed: move CURRENT_HASH into EARLIER_WINDOWS_BUILD_SCRIPTS \
             and update CURRENT_HASH"
        );
        assert!(
            !EARLIER_WINDOWS_BUILD_SCRIPTS.contains(&CURRENT_HASH),
            "the current text is not an earlier one"
        );
    }

    /// Templates' shipped `main.rs` files must not panic on recoverable input: no `f32::clamp` with
    /// window-derived bounds, no `println!` (broken pipe), no `env::args()` (non-UTF-8), no
    /// unchecked `thread::spawn`.
    #[test]
    fn template_mains_avoid_known_panic_paths() {
        let text = |id: &str, path: &str| {
            template_files(id)
                .unwrap_or(&[])
                .iter()
                .find(|(p, _)| *p == path)
                .map_or("", |(_, t)| *t)
        };
        let quick = text("quick", "src/main.rs");
        assert!(quick.contains("fn clamp_within"));
        assert!(
            !quick.contains(".clamp(area"),
            "window-derived clamp bounds can invert"
        );
        assert!(
            !quick.contains("m.ball.x.clamp("),
            "window-derived clamp bounds can invert"
        );
        for path in ["server/src/main.rs", "client/src/main.rs"] {
            let src = text("net_game", path);
            assert!(
                !src.lines().any(|l| l.trim_start().starts_with("println!(")),
                "{path}: println!"
            );
            assert!(
                !src.contains("std::env::args()"),
                "{path}: env::args panics on non-UTF-8"
            );
            assert!(
                !src.contains("std::thread::spawn"),
                "{path}: thread::spawn panics on EAGAIN"
            );
            assert!(src.contains("args_os()"), "{path}: args_os");
            assert!(src.contains("fn say("), "{path}: say helper");
        }
    }

    /// The text of one file of a template (empty when the template or path is unknown).
    fn file(id: &str, path: &str) -> &'static str {
        template_files(id)
            .unwrap_or(&[])
            .iter()
            .find(|(p, _)| *p == path)
            .map_or("", |(_, t)| *t)
    }

    /// Load restore runs before checkpoint tracking in every game template: a load applied in
    /// `Last` lands in `PlayerCheckpoint`, and tracking first would copy the live pose over it.
    #[test]
    fn checkpoint_restore_runs_before_tracking() {
        for id in ["first_person", "third_person", "top_down"] {
            let lib = file(id, "game/src/lib.rs");
            let restore = lib.find("restore_checkpoint,").unwrap_or(usize::MAX);
            let track = lib.find("track_checkpoint,").unwrap_or(0);
            assert!(restore < track, "{id}: restore before track");
            assert!(lib.contains(".chain()"), "{id}: the Update systems are ordered");
        }
    }

    /// The mouse and the right stick keep their own look scales (rad per pixel vs rad per
    /// second), the mouse Y is negated (screen Y grows down, pitch grows up) and the stick is a
    /// rate scaled by the frame time.
    #[test]
    fn look_input_is_radians_per_pixel_and_per_second_with_mouse_y_inverted() {
        for id in ["first_person", "third_person"] {
            let lib = file(id, "game/src/lib.rs");
            assert!(
                lib.contains("-MOUSE_SENSITIVITY"),
                "{id}: mouse Y negated (pitch is up-positive)"
            );
            assert!(lib.contains("ScaleByDelta"), "{id}: the stick is a rate");
            assert!(lib.contains("STICK_TURN_RATE"), "{id}: stick rate constant");
        }
    }

    /// Top Down's stand-in box reaches the floor (the capsule centre rests at 1.2 m), the
    /// template ships `assets/config/weather.ron` like the others, and every workspace template
    /// stamps the engine line.
    #[test]
    fn stand_in_box_weather_and_engine_line_regressions() {
        assert!(
            file("top_down", "game/src/lib.rs").contains("cube_mesh(Vec3::new(0.3, 1.2, 0.3))"),
            "top_down box reaches the floor"
        );
        assert!(
            template_files("top_down")
                .unwrap_or(&[])
                .iter()
                .any(|(p, _)| *p == "assets/config/weather.ron"),
            "top_down ships assets/config/weather.ron"
        );
        for id in ["blank", "first_person", "third_person", "top_down", "net_game"] {
            let toml = file(id, "Regia.toml");
            assert!(
                toml.contains("engine = \"{{engine_line}}\""),
                "{id} stamps the engine line"
            );
        }
        let weather = include_str!("../templates/weather.ron");
        assert!(weather.contains("precipitation: true"));
        assert!(weather.contains("surface: true"));
        assert!(weather.contains("scene_wind: true"));
        for id in ["blank", "first_person", "third_person", "top_down"] {
            let files = template_files(id).unwrap_or(&[]);
            assert!(
                files
                    .iter()
                    .any(|(path, body)| { *path == "assets/config/weather.ron" && *body == weather }),
                "{id} ships weather.ron with the three effects on"
            );
        }
        for id in ["net_game", "quick"] {
            let files = template_files(id).unwrap_or(&[]);
            assert!(
                files.iter().all(|(path, _)| *path != "assets/config/weather.ron"),
                "{id} ships no weather file"
            );
        }
    }

    /// The net-game bot aims at the nearest other avatar (not the first in the query), a held
    /// fire button is judged once per cooldown, and `--ticks 0` runs no server step.
    #[test]
    fn net_game_bot_targets_the_nearest_and_zero_ticks_runs_no_step() {
        let lib = file("net_game", "game/src/lib.rs");
        assert!(lib.contains("total_cmp(&b.distance(here))"));
        assert!(lib.contains("fn shot_allowed"));
        assert!(file("net_game", "server/src/main.rs").contains("args.ticks == Some(0)"));
    }

    /// The net game's server steps an avatar from an untrusted command: a non-finite stick stands
    /// still and a non-finite aim is no shot (the template's own test,
    /// `a_non_finite_command_does_not_poison_the_position`, runs in a generated project).
    #[test]
    fn net_game_ignores_non_finite_commands() {
        let lib = file("net_game", "game/src/lib.rs");
        assert!(
            lib.contains("if !dir.is_finite()"),
            "step_position guards the stick"
        );
        assert!(
            lib.contains("c.value.aim.is_finite()"),
            "judge_shots guards the aim"
        );
        assert!(lib.contains("fn a_non_finite_command_does_not_poison_the_position"));
    }

    /// The quick paddle moves toward a touch by at most one frame's travel, never overshooting.
    #[test]
    fn quick_paddle_follows_a_touch_at_paddle_speed() {
        assert!(file("quick", "src/main.rs").contains("clamp(-reach, reach)"));
    }

    /// ADR-2954: every game a template writes quits after `--exit-after-frames N` (the book's
    /// "Checking that it starts", `cargo xtask walkthrough`, `templates-check`) and the launchers
    /// and the net client end the process with the app's exit code. The quick app parses the
    /// flag in its model (see its own tests) and keeps `run()`'s exit, so it is held to the flag
    /// only (integration of the walkthrough lane onto the newer quick and net client mains).
    #[test]
    fn every_game_main_handles_exit_after_frames_and_returns_the_exit_code() {
        let mut games = 0;
        for &id in TEMPLATE_IDS {
            for (path, text) in template_files(id).unwrap_or(&[]) {
                if !matches!(
                    *path,
                    "launcher/src/main.rs" | "src/main.rs" | "client/src/main.rs"
                ) {
                    continue;
                }
                games += 1;
                assert!(
                    text.contains("\"--exit-after-frames\""),
                    "{id}/{path}: parses the flag"
                );
                if *path == "src/main.rs" {
                    continue;
                }
                assert!(
                    text.contains("AppExit::Success"),
                    "{id}/{path}: ends the app after the frames"
                );
                assert!(
                    text.contains("fn main() -> ExitCode"),
                    "{id}/{path}: the process reports the app's exit code"
                );
                assert!(
                    text.contains("AppExit::Error(code)"),
                    "{id}/{path}: errors reach the exit code"
                );
            }
        }
        assert_eq!(games, 6, "four launchers, the quick app and the net client");
    }

    /// Spec 61 § 2.3 and § 10 test 1: `blank` and `empty` are one template.
    #[test]
    fn blank_aliases_empty() {
        assert_eq!(template_files("empty"), template_files("blank"));
        assert!(template_files("empty").is_some());
    }

    /// Spec 61 § 2.1 `done`: a template sets `[project] startup_level` to a level it ships, so a
    /// packaged build never starts black. `quick` (no `Regia.toml`; `regia_quick` draws) and
    /// `net_game` (its client builds the view in code, the level is the server's rotation) build
    /// their scene in code instead (ADR-5643).
    #[test]
    fn every_template_with_a_manifest_starts_on_a_level_it_ships() {
        const SCENE_IN_CODE: &[&str] = &["quick", "net_game"];
        for id in TEMPLATE_IDS {
            let files = template_files(id).expect("listed id has files");
            let manifest = files
                .iter()
                .find(|(rel, _)| *rel == "Regia.toml")
                .map(|(_, t)| *t);
            if SCENE_IN_CODE.contains(id) {
                assert!(
                    manifest.is_none_or(|m| !m.contains("startup_level")),
                    "{id}: listed as scene-in-code but sets a startup level"
                );
                continue;
            }
            let manifest = manifest.unwrap_or_else(|| panic!("{id}: no Regia.toml"));
            let table: toml::Table = manifest.parse().expect("Regia.toml parses");
            let level = table["project"]["startup_level"]
                .as_str()
                .unwrap_or_else(|| panic!("{id}: no [project] startup_level"));
            let shipped = format!("assets/{level}");
            assert!(
                files.iter().any(|(rel, _)| *rel == shipped),
                "{id}: startup level {shipped} is not in the template"
            );
        }
    }

    /// Spec 61 § 10 test 3 and § 11: the shared name rule normalizes or refuses, and never yields a
    /// Rust or Java reserved word or a Windows device name.
    #[test]
    fn project_crate_names_follow_the_table() {
        let table: &[(&str, Option<&str>)] = &[
            ("My Game", Some("my_game")),
            ("  Spaced   Out  ", Some("spaced_out")),
            ("UPPER", Some("upper")),
            ("hello-world", Some("hello_world")),
            ("Café Racer", Some("caf_racer")),
            ("console", Some("console")),
            ("com10", Some("com10")),
            ("comx", Some("comx")),
            ("9lives", None),
            ("日本語", None),
            ("___", None),
            ("", None),
            ("fn", None),
            ("gen", None),
            ("class", None),
            ("a\"b", None),
            ("a\\b", None),
            ("two\nlines", None),
            ("con", None),
            ("Nul", None),
            ("AUX", None),
            ("prn", None),
            ("com0", None),
            ("COM9", None),
            ("lpt1", None),
        ];
        for (name, expected) in table {
            assert_eq!(project_crate_name(name).as_deref(), *expected, "{name:?}");
        }
        for word in RESERVED_NAMES {
            assert_eq!(project_crate_name(word), None, "{word}");
        }
    }

    /// A fresh scratch directory: pid plus a counter, never a fixed name.
    fn scratch(label: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-templates-{label}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// Spec 61 § 2.1 `location` and `generate`, § 10 test 2: a new or empty target is claimed, an
    /// occupied one is refused, and a failed write leaves the target as it was found.
    #[test]
    fn write_project_dir_claims_new_or_empty_targets_and_leaves_nothing_on_failure() {
        let parent = scratch("write");
        std::fs::write(parent.join("user.bin"), b"user").expect("user file");
        let files = |body: &str| vec![("Cargo.toml", body.to_owned()), ("src/lib.rs", String::new())];
        let failing = vec![
            ("occupied", "file".to_owned()),
            ("occupied/child", "no".to_owned()),
        ];

        // A new target: written; a failed write removes it whole.
        let fresh = parent.join("fresh");
        assert!(project_dir_is_free(&fresh));
        assert!(write_project_dir(&fresh, &failing).is_err());
        assert!(!fresh.exists(), "nothing remains");
        write_project_dir(&fresh, &files("a")).expect("new target");
        assert_eq!(std::fs::read(fresh.join("Cargo.toml")).expect("written"), b"a");

        // An occupied target is refused and left alone.
        assert!(!project_dir_is_free(&fresh));
        assert_eq!(
            write_project_dir(&fresh, &files("b")),
            Err(ProjectDirError::Exists(fresh.clone()))
        );
        assert_eq!(std::fs::read(fresh.join("Cargo.toml")).expect("kept"), b"a");

        // An empty directory is taken; a failed write empties it again and keeps it.
        let empty = parent.join("empty");
        std::fs::create_dir(&empty).expect("empty dir");
        assert!(project_dir_is_free(&empty));
        assert!(write_project_dir(&empty, &failing).is_err());
        assert!(empty.is_dir(), "the user's folder stays");
        assert_eq!(
            std::fs::read_dir(&empty).expect("read").count(),
            0,
            "and is empty again"
        );
        write_project_dir(&empty, &files("c")).expect("empty target");
        assert_eq!(std::fs::read(empty.join("Cargo.toml")).expect("written"), b"c");

        // A file is not a free target.
        assert!(!project_dir_is_free(&parent.join("user.bin")));
        assert_eq!(
            std::fs::read(parent.join("user.bin")).expect("user file kept"),
            b"user"
        );
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// A symlink to an empty directory is not taken: the project would land outside the parent.
    #[cfg(unix)]
    #[test]
    fn a_symlink_to_an_empty_directory_is_not_a_free_target() {
        let parent = scratch("symlink");
        let elsewhere = parent.join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("dir");
        let link = parent.join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).expect("symlink");
        assert!(!project_dir_is_free(&link));
        assert_eq!(
            write_project_dir(&link, &[("Cargo.toml", String::new())]),
            Err(ProjectDirError::Exists(link.clone()))
        );
        assert_eq!(std::fs::read_dir(&elsewhere).expect("read").count(), 0);
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// ADR-10370: every template ships the workspace's directory-root remaps, and the
    /// compiled-in text names no private host (it is linked into the `regia` binary).
    /// The home directory is appended last, per target, only when a project is written.
    #[test]
    fn every_template_ships_root_remaps_and_home_is_appended_last() {
        const ROOTS: [&str; 7] = ["/home", "/media", "/srv", "/models", "/root", "/mnt", "/Users"];
        // The gate matches these with a trailing slash. The shipped flag text uses `=`.
        for leaked in ["/home/", "/media/", "/srv/", "/models/"] {
            assert!(
                !PATH_REMAP_CONFIG.contains(leaked),
                "compiled-in config contains {leaked}"
            );
        }
        let targets = PATH_REMAP_CONFIG.matches("rustflags = [").count();
        assert!(targets >= 4, "the workspace targets, got {targets}");
        for id in TEMPLATE_IDS {
            let files = template_files(id).unwrap_or(&[]);
            let config = files
                .iter()
                .find(|(path, _)| *path == ".cargo/config.toml")
                .map(|(_, text)| *text);
            assert_eq!(config, Some(PATH_REMAP_CONFIG), "{id}");
            for root in ROOTS {
                assert!(
                    PATH_REMAP_CONFIG.contains(&format!("--remap-path-prefix={root}=/builder")),
                    "{id} missing {root}"
                );
            }
        }
        let home = std::path::Path::new("/home/player");
        let written = with_home_path_remap(PATH_REMAP_CONFIG, home).expect("remap");
        assert_eq!(written.matches("rustflags = [").count(), targets);
        let flag = home_remap_flag(home).expect("flag");
        for array in written.split("rustflags = [").skip(1) {
            let body = array.split(']').next().unwrap_or("");
            let last = body
                .lines()
                .rev()
                .find(|line| line.contains("remap-path-prefix"))
                .unwrap_or("");
            assert!(last.contains(&flag), "last remap is {last}, want {flag}");
            let generic = body
                .find("--remap-path-prefix=/home=/builder")
                .unwrap_or(usize::MAX);
            let specific = body.find(&flag).unwrap_or(0);
            assert!(generic < specific, "the home prefix must follow the /home root");
        }
        assert!(home_remap_flag(std::path::Path::new("relative")).is_err());
        assert!(home_remap_flag(std::path::Path::new("/tmp/a\"b")).is_err());
        assert_eq!(
            home_remap_flag(std::path::Path::new("/tmp/player")).expect("abs"),
            "--remap-path-prefix=/tmp/player=/builder"
        );
        assert_eq!(
            escape_toml_basic(r"C:\Users\player"),
            r"C:\\Users\\player",
            "a Windows home is escaped for a TOML basic string"
        );
    }
}
