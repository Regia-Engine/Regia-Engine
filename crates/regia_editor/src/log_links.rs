// SPDX-License-Identifier: MPL-2.0
//! Clickable `path:line[:col]` and asset paths in the Output Log (`docs/specs/19-editor.md` § 2,
//! ADR-2696).
//!
//! [`parse_links`] finds the links of one log line. A file link is a path followed by `:line` or
//! `:line:col` whose last segment has an extension; an asset link is a path with an asset suffix and
//! no line number. Paths with spaces are found when they are quoted, start from a root (`/`, `./`,
//! `../`, `~/`, `C:\`, `C:/`, `\\`) or have a directory in each word before the file name's last word
//! (`assets/my models/a b.glb`). Where the text is ambiguous (`copied /tmp/a to src/b.rs:3`) the link
//! lists every reading, longest first, and the click takes the first that exists.
//!
//! [`open_clicked`] runs the code editor for a file link and queues an asset link for the shell,
//! which opens it as the Content Browser would ([`AssetAction`]). The command is the preference
//! `code.editor_command` ([`code_editor_command`]), then `[editor] code_editor` of the project's
//! `.regia/agent.toml` (`regia_agent::open_in_editor::code_editor_command`), then `$VISUAL` and
//! `$EDITOR`. It never goes through a shell. Agents cannot set that preference, so a request can
//! never pick the program.
//!
//! An entity GUID in its dashed text form is a link too ([`LinkTarget::Entity`], ADR-7789): the
//! click selects the entity of the open level and frames it ([`select_entity_by_guid`]).
//!
//! The agent surface is `editor.log_links` and `editor.log_open_link`
//! (`book/agents/viewport-tools.md`).

use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::log::ring::{self, LogQuery};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::editor_prefs::EditorPreferences;
use crate::viewport_agent::Handler;

/// The preference that names the code editor.
pub const CODE_EDITOR_PREF: &str = "code.editor_command";

/// Longest path considered, bytes: bounds the work per candidate.
const MAX_PATH_LEN: usize = 1024;
/// Most links kept per line.
const MAX_LINKS: usize = 32;
/// The part of a line that is searched, bytes (a compile error can carry a huge message).
const MAX_LINE_LEN: usize = 16 * 1024;
/// Largest line or column number read.
const MAX_LINE_NUMBER: u32 = 100_000_000;
/// Most space-separated pieces joined into one path.
const MAX_PATH_WORDS: usize = 12;

/// Extensions that make a bare `name.ext:12` a file link. With a directory in the path any extension
/// starting with a letter counts.
const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "wgsl", "toml", "ron", "json", "md", "c", "h", "cc", "cpp", "hpp", "py", "js", "ts", "glsl",
    "hlsl", "kt", "java", "yaml", "yml", "txt", "sh", "ps1",
];

/// Suffixes that make a path without a line number an asset link.
const ASSET_SUFFIXES: &[&str] = &[
    ".material.ron",
    ".prefab.ron",
    ".variant.ron",
    ".level.ron",
    ".animgraph.ron",
    ".matgraph.ron",
    ".matfunc.ron",
    ".matinst.ron",
    ".seq.ron",
    ".shots.ron",
    ".ui.ron",
    ".uianim.ron",
    ".pgraph.ron",
    ".pgvar.ron",
    ".ability.ron",
    ".effect.ron",
    ".attrs.ron",
    ".bt.ron",
    ".statetree.ron",
    ".montage.ron",
    ".montagevar.ron",
    ".cue.ron",
    ".data.ron",
    ".table.ron",
    ".table.csv",
    ".curves.ron",
    ".curves.csv",
    ".chooser.ron",
    ".conv.ron",
    ".vfx.ron",
    ".cloth.ron",
    ".soft.ron",
    ".motiondb.ron",
    ".biome.ron",
    ".logic.ron",
    ".video.ron",
    ".webm",
    ".mkv",
    ".theme.ron",
    ".gltf",
    ".glb",
    ".fbx",
    ".obj",
    ".png",
    ".jpg",
    ".jpeg",
    ".tga",
    ".bmp",
    ".hdr",
    ".exr",
    ".ktx2",
    ".wav",
    ".ogg",
    ".flac",
    ".mp3",
];

/// What a link opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// A source file at a line in the code editor.
    File {
        /// The 1-based line.
        line: u32,
        /// The 1-based column.
        col: Option<u32>,
    },
    /// An asset, the way the Content Browser opens it.
    Asset,
    /// An entity of the open level by its GUID ([`LogLink::path`] is the GUID): selected and framed
    /// (ADR-7789).
    Entity,
}

/// One link in a log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLink {
    /// Byte offset of the link's first character.
    pub start: usize,
    /// Byte offset just past the link (including `:line:col`).
    pub end: usize,
    /// The path as written; the readings of the text, longest first ([`Self::path`] is the first).
    pub paths: Vec<String>,
    /// What it opens.
    pub target: LinkTarget,
}

impl LogLink {
    /// The link's preferred path.
    #[must_use]
    pub fn path(&self) -> &str {
        self.paths.first().map_or("", String::as_str)
    }
}

/// `true` for bytes that end a path when they come before it.
fn is_opening(byte: u8) -> bool {
    matches!(byte, b'(' | b'[' | b'{' | b'<' | b'=' | b',' | b';')
}

/// The offset of a path root inside `word`: its start, or later for `at:/x` and `(C:\x`.
fn root_in_word(word: &str) -> Option<usize> {
    if word.contains("://") && !word.contains("file://") {
        return None;
    }
    let bytes = word.as_bytes();
    let rooted_at = |i: usize| {
        let rest = &bytes[i..];
        match rest {
            [b'/', ..] | [b'~', b'/', ..] | [b'.', b'/', ..] | [b'.', b'.', b'/', ..] => true,
            [b'.', b'\\', ..] | [b'.', b'.', b'\\', ..] | [b'\\', b'\\', ..] => true,
            [letter, b':', b'\\' | b'/', ..] => letter.is_ascii_alphabetic(),
            _ => false,
        }
    };
    (0..bytes.len()).find(|&i| {
        word.is_char_boundary(i)
            && rooted_at(i)
            && (i == 0 || is_opening(bytes[i - 1]) || bytes[i - 1] == b':')
    })
}

/// Whether `path` can be a file path at all.
fn plausible_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('-')
        && !path.contains("://")
        && !path.contains("::")
        && !path
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '<' | '>' | '|' | '*' | '?' | '`'))
}

/// `path`'s last segment has an extension worthy of a `:line`: one starting with a letter, and with
/// no directory in the path one of the source extensions.
fn has_source_extension(path: &str) -> bool {
    let (dir, name) = match path.rfind(['/', '\\']) {
        Some(at) => (true, &path[at + 1..]),
        None => (false, path),
    };
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    let good = !extension.is_empty()
        && extension.len() <= 12
        && extension.starts_with(|c: char| c.is_ascii_alphabetic())
        && extension.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !good || stem.is_empty() && !dir {
        return false;
    }
    dir || SOURCE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
}

/// Where the path that ends at byte `end` of `text` can start (not before `floor`), longest reading
/// first: inside an open quote everything up to it; otherwise from the nearest word holding a path
/// root, joining the spaces between; from each unbroken word back that names a directory; and the
/// last word alone.
fn readings(text: &str, floor: usize, end: usize) -> Vec<(usize, String)> {
    let floor = floor.max(end.saturating_sub(MAX_PATH_LEN));
    let mut floor = floor;
    while !text.is_char_boundary(floor) {
        floor += 1;
    }
    let window = &text[floor..end];
    // An unclosed quote before the end: the path is inside it.
    for quote in ['"', '`', '\''] {
        let count = window.matches(quote).count();
        if count % 2 == 1
            && let Some(at) = window.rfind(quote)
            && (quote != '\'' || at == 0 || !window.as_bytes()[at - 1].is_ascii_alphanumeric())
        {
            let start = floor + at + 1;
            return vec![(start, text[start..end].to_owned())];
        }
    }
    // Words back from the end; a closing quote or bracket before them ends the search.
    let mut words: Vec<(usize, &str)> = Vec::new();
    let mut cursor = window.len();
    while cursor > 0 && words.len() < MAX_PATH_WORDS {
        let head = &window[..cursor];
        let word_start = head
            .rfind(char::is_whitespace)
            .map_or(0, |at| at + head[at..].chars().next().map_or(1, char::len_utf8));
        words.push((word_start, &window[word_start..cursor]));
        let before = head[..word_start].trim_end();
        if before.len() == cursor || before.is_empty() {
            break;
        }
        cursor = before.len();
    }
    let mut out = Vec::new();
    let Some(&(last_start, last)) = words.first() else {
        return out;
    };
    let after_opening = |word: &str| {
        word.rfind(|c: char| c.is_ascii() && is_opening(c as u8) || c == '\'')
            .map_or(0, |at| at + 1)
    };
    let last_offset = root_in_word(last).unwrap_or_else(|| after_opening(last));
    let last_path_start = floor + last_start + last_offset;
    out.push((last_path_start, text[last_path_start..end].to_owned()));
    if root_in_word(last).is_none() {
        // Back through the words: those naming directories continue a relative path with spaces
        // (`assets/my models/a b.glb`), and the nearest root starts an absolute one.
        let mut unbroken = true;
        for &(word_start, word) in words.iter().skip(1) {
            let root = root_in_word(word);
            // A complete path of its own (`/tmp/a.txt to src/b.rs:3`) does not start this one.
            if has_source_extension(word) {
                break;
            }
            if let Some(offset) = root {
                let start = floor + word_start + offset;
                out.push((start, text[start..end].to_owned()));
                break;
            }
            unbroken = unbroken && word.contains(['/', '\\']);
            if unbroken {
                let start = floor + word_start + after_opening(word);
                out.push((start, text[start..end].to_owned()));
            }
        }
    }
    // Longest first.
    out.reverse();
    out
}

/// Reads `:line` or `:line:col` at byte `at` (the first colon). Returns the numbers and the offset
/// just past them.
fn line_suffix(bytes: &[u8], at: usize) -> Option<(u32, Option<u32>, usize)> {
    let number = |from: usize| -> Option<(u32, usize)> {
        let mut end = from;
        let mut value: u32 = 0;
        while let Some(digit) = bytes.get(end).filter(|b| b.is_ascii_digit()) {
            value = value.checked_mul(10)?.checked_add(u32::from(digit - b'0'))?;
            end += 1;
        }
        (end > from && (1..=MAX_LINE_NUMBER).contains(&value)).then_some((value, end))
    };
    let (line, end) = number(at + 1)?;
    let (col, end) = match bytes.get(end) {
        Some(b':') => match number(end + 1) {
            Some((col, past)) => (Some(col), past),
            None => (None, end),
        },
        _ => (None, end),
    };
    // `file.rs:12abc` and `file.rs:12.5` are not lines.
    let after = bytes.get(end).copied();
    if after.is_some_and(|b| {
        b.is_ascii_alphanumeric() || b == b'.' && bytes.get(end + 1).is_some_and(u8::is_ascii_digit)
    }) {
        return None;
    }
    Some((line, col, end))
}

/// The links of one log line, in order, not overlapping.
#[must_use]
pub fn parse_links(text: &str) -> Vec<LogLink> {
    let mut limit = text.len().min(MAX_LINE_LEN);
    while !text.is_char_boundary(limit) {
        limit -= 1;
    }
    let text = &text[..limit];
    let bytes = text.as_bytes();
    let mut links: Vec<LogLink> = Vec::new();
    // File links: a colon and digits, with a path before them.
    let mut floor = 0;
    for (at, _) in text.match_indices(':') {
        if at < floor || links.len() >= MAX_LINKS {
            continue;
        }
        let Some((line, col, end)) = line_suffix(bytes, at) else {
            continue;
        };
        let paths: Vec<(usize, String)> = readings(text, floor, at)
            .into_iter()
            .filter(|(_, path)| plausible_path(path) && has_source_extension(path))
            .collect();
        let Some(&(start, _)) = paths.first() else {
            continue;
        };
        links.push(LogLink {
            start,
            end,
            paths: paths.into_iter().map(|(_, path)| path).collect(),
            target: LinkTarget::File { line, col },
        });
        floor = end;
    }
    // Asset links: a suffix at the end of a word, outside the file links.
    let lower = text.to_ascii_lowercase();
    let mut assets: Vec<LogLink> = Vec::new();
    for suffix in ASSET_SUFFIXES {
        for (at, _) in lower.match_indices(suffix) {
            let end = at + suffix.len();
            if bytes
                .get(end)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
            {
                continue;
            }
            // `.ron` suffixes are also read as a shorter asset: the longest match wins below.
            if links.iter().any(|l| at < l.end && end > l.start) {
                continue;
            }
            let floor = links
                .iter()
                .filter(|l| l.end <= at)
                .map(|l| l.end)
                .max()
                .unwrap_or(0);
            let paths: Vec<(usize, String)> = readings(text, floor, end)
                .into_iter()
                .filter(|(_, path)| plausible_path(path))
                .collect();
            if let Some(&(start, _)) = paths.first() {
                assets.push(LogLink {
                    start,
                    end,
                    paths: paths.into_iter().map(|(_, path)| path).collect(),
                    target: LinkTarget::Asset,
                });
            }
        }
    }
    assets.sort_by_key(|l| (l.start, std::cmp::Reverse(l.end)));
    for asset in assets {
        if links.len() >= MAX_LINKS {
            break;
        }
        if !links.iter().any(|l| asset.start < l.end && asset.end > l.start) {
            links.push(asset);
        }
    }
    // Entity GUIDs (`8-4-4-4-12` hex digits): outside the file and asset links.
    for link in entity_links(text) {
        if links.len() >= MAX_LINKS {
            break;
        }
        if !links.iter().any(|l| link.start < l.end && link.end > l.start) {
            links.push(link);
        }
    }
    links.sort_by_key(|l| l.start);
    links
}

/// The entity GUID links of `text`: the dashed text form of a GUID (`67e55044-10b1-426f-9247-bb680e5fe0c8`,
/// either case) with no letter, digit, `-` or `_` touching it.
fn entity_links(text: &str) -> Vec<LogLink> {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    const LEN: usize = 36;
    let bytes = text.as_bytes();
    let joined = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
    let mut out = Vec::new();
    let mut at = 0;
    while at + LEN <= bytes.len() {
        if !bytes[at].is_ascii_hexdigit() || (at > 0 && joined(bytes[at - 1])) {
            at += 1;
            continue;
        }
        let mut cursor = at;
        let mut ok = true;
        for (i, len) in GROUPS.iter().enumerate() {
            if i > 0 {
                ok &= bytes[cursor] == b'-';
                cursor += 1;
            }
            ok &= bytes[cursor..cursor + len].iter().all(u8::is_ascii_hexdigit);
            cursor += len;
            if !ok {
                break;
            }
        }
        if ok && !bytes.get(cursor).copied().is_some_and(joined) {
            out.push(LogLink {
                start: at,
                end: cursor,
                paths: vec![text[at..cursor].to_ascii_lowercase()],
                target: LinkTarget::Entity,
            });
            at = cursor;
        } else {
            at += 1;
        }
        if out.len() >= MAX_LINKS {
            break;
        }
    }
    out
}

/// Selects the entity of the open level whose [`regia_scene::prelude::EntityGuid`] is `guid` and
/// frames it in the editor camera, as a click on the entity in the Outliner and *Frame selection*
/// do.
///
/// # Errors
///
/// `invalid_params` for text that is not a GUID; `F0003` (not found) when no entity has it.
pub fn select_entity_by_guid(world: &mut World, guid: &str) -> Result<Entity, RpcError> {
    let parsed: regia_core::id::Guid = guid
        .parse()
        .map_err(|_| RpcError::invalid_params(format!("{guid:?} is not an entity GUID")))?;
    let mut query = world.query::<(Entity, &regia_scene::prelude::EntityGuid)>();
    let found = query
        .iter(world)
        .find(|(_, id)| id.0 == parsed)
        .map(|(entity, _)| entity);
    let Some(entity) = found else {
        return Err(RpcError::not_found(
            format!("no entity {guid} in the open level"),
            "the GUID may name an asset, or an entity of another level",
        ));
    };
    world
        .get_resource_or_insert_with(crate::selection::Selection::default)
        .select(entity, crate::selection::SelectMode::Replace);
    crate::command_palette::frame_selection(world);
    Ok(entity)
}

/// Splits a command line into words: spaces separate, `"..."` and `'...'` group. Never a shell.
fn split_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            None => word.push(c),
        }
    }
    if started || !word.is_empty() {
        words.push(word);
    }
    words
}

/// The command `template` runs for a file: `{file}`, `{line}` and `{col}` substitute per word. With
/// no `{file}`, the named editors get their own way to say where (`code -g f:l:c`, `zed f:l:c`) and
/// any other program `+line file`, which `vi`, `nano` and `emacs` read. `env` reads the environment
/// (for `$EDITOR`, `$VISUAL` as a word of the template).
#[must_use]
pub fn expand_command(
    template: &str,
    file: &Path,
    line: u32,
    col: Option<u32>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    let file = file.display().to_string();
    let col = col.unwrap_or(1);
    let mut words: Vec<String> = Vec::new();
    for word in split_words(template) {
        match word.strip_prefix('$').map(env) {
            Some(Some(value)) => words.extend(split_words(&value)),
            Some(None) => {}
            None => words.push(word),
        }
    }
    let Some(program) = words.first().cloned() else {
        return Vec::new();
    };
    if words.iter().any(|w| w.contains("{file}")) {
        return words
            .iter()
            .map(|word| {
                word.replace("{file}", &file)
                    .replace("{line}", &line.to_string())
                    .replace("{col}", &col.to_string())
            })
            .collect();
    }
    let at = format!("{file}:{line}:{col}");
    let name = Path::new(&program)
        .file_stem()
        .map_or_else(String::new, |n| n.to_string_lossy().to_ascii_lowercase());
    match name.as_str() {
        "code" | "code-insiders" | "codium" | "cursor" => words.extend(["-g".to_owned(), at]),
        "zed" | "subl" | "sublime_text" => words.push(at),
        _ => words.extend([format!("+{line}"), file]),
    }
    words
}

/// The configured code editor command for `file`: the preference [`CODE_EDITOR_PREF`], else
/// `[editor] code_editor` of the project's `.regia/agent.toml`.
///
/// # Errors
///
/// `open_in_editor_no_code_editor` when neither is set, or the preference expands to nothing.
pub fn code_editor_command(
    world: &World,
    root: &Path,
    file: &Path,
    line: u32,
    col: Option<u32>,
) -> Result<Vec<String>, RpcError> {
    let template = world
        .get_resource::<EditorPreferences>()
        .map(|prefs| prefs.text(CODE_EDITOR_PREF))
        .unwrap_or_default();
    let env = |name: &str| regia_core::brand::env_var_os(name).map(|v| v.to_string_lossy().into_owned());
    if !template.trim().is_empty() {
        let command = expand_command(&template, file, line, col, &env);
        if !command.is_empty() {
            return Ok(command);
        }
    }
    regia_agent::open_in_editor::code_editor_command(root, file, u64::from(line))
}

/// [`code_editor_command`], then the environment's `$VISUAL` and `$EDITOR`: a click in the log works
/// on a machine with nothing configured.
///
/// # Errors
///
/// `open_in_editor_no_code_editor` when none of them names a program.
pub fn click_command(
    world: &World,
    root: &Path,
    file: &Path,
    line: u32,
    col: Option<u32>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<String>, RpcError> {
    let err = match code_editor_command(world, root, file, line, col) {
        Ok(command) => return Ok(command),
        Err(err) => err,
    };
    let command = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|name| env(name).filter(|v| !v.trim().is_empty()))
        .map(|value| expand_command(&value, file, line, col, env))
        .filter(|command| !command.is_empty());
    command.ok_or(err)
}

/// How an asset path opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetAction {
    /// In its editor panel (every kind of [`crate::asset_open::OPEN_KINDS`] but the three below).
    Editor,
    /// In the Asset Viewer (a mesh, texture or sound).
    Viewer,
    /// In prefab editing.
    Prefab,
    /// As the open level (refused while the current level has unsaved edits).
    Level,
    /// In the code editor (other text assets).
    CodeEditor,
}

impl AssetAction {
    /// The wire name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Editor => "editor",
            Self::Viewer => "viewer",
            Self::Prefab => "prefab",
            Self::Level => "level",
            Self::CodeEditor => "code_editor",
        }
    }
}

/// How the asset at `path` opens: by the asset-open dispatcher's kind table (ADR-7725), else in the
/// code editor.
#[must_use]
pub fn asset_action(path: &str) -> AssetAction {
    use crate::asset_open::OpenRoute;
    match crate::asset_open::kind_of(path).map(|kind| kind.route) {
        Some(OpenRoute::Prefab) => AssetAction::Prefab,
        Some(OpenRoute::Level) => AssetAction::Level,
        Some(OpenRoute::Viewer) => AssetAction::Viewer,
        Some(OpenRoute::Material | OpenRoute::Method { .. }) => AssetAction::Editor,
        None => AssetAction::CodeEditor,
    }
}

/// An existing file the path names: as it is when absolute, else under the project, its `game/`
/// crate or its `assets/` directory. Candidates are tried in that order.
#[must_use]
pub fn resolve_path(root: &Path, path: &str) -> Option<PathBuf> {
    let written = Path::new(path);
    let home = path
        .strip_prefix("~/")
        .and_then(|rest| regia_core::brand::env_var_os("HOME").map(|home| PathBuf::from(home).join(rest)));
    if let Some(home) = home {
        return home.is_file().then_some(home);
    }
    if written.is_absolute() {
        return written.is_file().then(|| written.to_path_buf());
    }
    ["", "game", "assets"]
        .iter()
        .map(|dir| root.join(dir).join(written))
        .find(|candidate| candidate.is_file())
}

/// The first reading of `link` that names an existing file.
#[must_use]
pub fn resolve_link(root: &Path, link: &LogLink) -> Option<PathBuf> {
    link.paths.iter().find_map(|path| resolve_path(root, path))
}

/// Assets the log asked to open; the shell drains it on its next frame (the panels' open requests
/// live outside the world).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct AssetOpenRequests {
    /// Project-relative asset paths, oldest first.
    pub paths: Vec<String>,
}

/// Runs what `link` opens: resolves it, then starts the code editor or queues the asset. Returns the
/// `editor.log_open_link` reply. With `dry_run`, only reports.
///
/// # Errors
///
/// `F0003` when no reading of the path exists; `open_in_editor_no_code_editor`; a failure to start
/// the program.
pub fn open_link(
    world: &mut World,
    link: &LogLink,
    dry_run: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Value, RpcError> {
    if link.target == LinkTarget::Entity {
        let entity = if dry_run {
            None
        } else {
            Some(select_entity_by_guid(world, link.path())?)
        };
        return Ok(json!({
            "opened": "entity",
            "guid": link.path(),
            "entity": entity.map(Entity::to_bits),
        }));
    }
    let root = regia_agent::req_path::project_root(world);
    let Some(full) = resolve_link(&root, link) else {
        return Err(RpcError::not_found(
            format!("no file {}", link.path()),
            "the path is relative to the project, its game/ crate or its assets/ directory",
        ));
    };
    let (line, col) = match link.target {
        LinkTarget::File { line, col } => (line, col),
        LinkTarget::Entity => (1, None),
        LinkTarget::Asset => {
            let action = asset_action(&full.to_string_lossy());
            if action != AssetAction::CodeEditor {
                let rel = full
                    .strip_prefix(&root)
                    .map_or_else(|_| full.clone(), Path::to_path_buf)
                    .to_string_lossy()
                    .replace('\\', "/");
                if !dry_run {
                    world
                        .get_resource_or_insert_with(AssetOpenRequests::default)
                        .paths
                        .push(rel.clone());
                }
                return Ok(json!({ "opened": "asset", "action": action.name(), "path": rel }));
            }
            (1, None)
        }
    };
    let command = click_command(world, &root, &full, line, col, env)?;
    if !dry_run {
        crate::agent_changes::spawn_code_editor(&root, &command)?;
    }
    Ok(json!({
        "opened": "file",
        "path": full.display().to_string(),
        "line": line,
        "col": col,
        "command": command,
    }))
}

/// Opens the queued [`AssetOpenRequests`] the way a double click in the Content Browser does
/// (`crate::asset_open::open_in_shell`); a level waits for its save (the click must not throw away
/// unsaved edits). Returns a status line when something could not open. The shell calls it before
/// the panels draw.
pub fn route_asset_requests(
    world: &mut World,
    content: &mut crate::content::ContentBrowserState,
) -> Option<String> {
    let requests = world.remove_resource::<AssetOpenRequests>()?;
    let mut status = None;
    for rel in requests.paths {
        if let Some(refused) = crate::asset_open::open_in_shell(world, content, &rel) {
            status = Some(refused);
        }
    }
    status
}

/// A click on a link in the panel: opens it and returns a status line for the panel.
pub fn open_clicked(world: &mut World, link: &LogLink) -> String {
    let env = |name: &str| regia_core::brand::env_var_os(name).map(|v| v.to_string_lossy().into_owned());
    match open_link(world, link, false, &env) {
        Ok(_) if link.target == LinkTarget::Entity => format!("Selected entity {}", link.path()),
        Ok(reply) => format!(
            "Opened {}",
            reply.get("path").and_then(Value::as_str).unwrap_or_default()
        ),
        Err(err) => {
            tracing::warn!(target: "regia_editor", "log link: {}", err.message);
            err.message
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

fn link_json(link: &LogLink) -> Value {
    let (kind, line, col) = match link.target {
        LinkTarget::File { line, col } => ("file", Some(line), col),
        LinkTarget::Asset => ("asset", None, None),
        LinkTarget::Entity => ("entity", None, None),
    };
    json!({
        "kind": kind,
        "start": link.start,
        "end": link.end,
        "path": link.path(),
        "readings": link.paths,
        "line": line,
        "col": col,
    })
}

/// `editor.log_links`.
fn log_links_method(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(text) = params.get("text").and_then(Value::as_str) {
        let links: Vec<Value> = parse_links(text).iter().map(link_json).collect();
        return Ok(json!({ "links": links }));
    }
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(200, |n| n.clamp(1, 2000)) as usize;
    let query = LogQuery {
        limit: Some(limit),
        ..LogQuery::default()
    };
    let (records, _) = ring::recent(&query);
    let links: Vec<Value> = records
        .iter()
        .flat_map(|record| {
            let seq = record.seq;
            parse_links(&record.message).into_iter().map(move |link| {
                let mut row = link_json(&link);
                row["seq"] = json!(seq);
                row
            })
        })
        .collect();
    Ok(json!({ "links": links }))
}

/// `editor.log_open_link`: a request stays inside the project, whatever the log says.
fn log_open_link_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`text` (the log line, or just a path:line) is required"))?;
    let index = params.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
    let dry_run = params.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let links = parse_links(text);
    let Some(mut link) = links.get(index).cloned() else {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "log_link_missing",
            format!("the text has {} links; there is no index {index}", links.len()),
            "see editor.log_links",
        ));
    };
    if link.target == LinkTarget::Entity {
        return open_link(world, &link, dry_run, &|_: &str| None);
    }
    let root = regia_agent::req_path::project_root(world);
    let inside: Vec<String> = link
        .paths
        .iter()
        .filter_map(|path| project_relative(&root, path))
        .filter(|rel| regia_agent::req_path::project_path_in(&root, "text", rel).is_ok())
        .collect();
    if inside.is_empty() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "log_link_outside_project",
            format!("{} is outside the project", link.path()),
            "agents open project files; the panel's click opens the rest",
        ));
    }
    link.paths = inside;
    let env = |name: &str| regia_core::brand::env_var_os(name).map(|v| v.to_string_lossy().into_owned());
    open_link(world, &link, dry_run, &env)
}

/// `path` relative to the project: as written when relative, stripped when absolute under `root`.
fn project_relative(root: &Path, path: &str) -> Option<String> {
    let written = Path::new(path);
    if !written.is_absolute() {
        return (!path.starts_with("~/")).then(|| path.replace('\\', "/"));
    }
    written
        .strip_prefix(root)
        .ok()
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
}

/// The Output Log link agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.log_links",
                tool: "log_links",
                description: "The clickable links in log text, as the Output Log parses them: path:line[:col] file links, asset paths and entity GUIDs (kind entity), with every reading of an ambiguous path. Without text, the links of the recent log records.",
                params: &[
                    ParamSchema {
                        name: "text",
                        kind: "string",
                        required: false,
                        description: "A log line to parse. Omitted: the recent records.",
                    },
                    ParamSchema {
                        name: "limit",
                        kind: "number",
                        required: false,
                        description: "Records searched without text (default 200, cap 2000).",
                    },
                ],
                result: "{ links: [{ kind, start, end, path, readings, line, col, seq? }] }",
                mutating: false,
                permission: Permission::Read,
            },
            log_links_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.log_open_link",
                tool: "log_open_link",
                description: "Opens a link of log text as a click in the Output Log does: a file in the owner's code editor at its line, an asset as the Content Browser opens it, an entity GUID selects and frames that entity. Project paths only; a request can never pick the program.",
                params: &[
                    ParamSchema {
                        name: "text",
                        kind: "string",
                        required: true,
                        description: "The log line, or just a path:line.",
                    },
                    ParamSchema {
                        name: "index",
                        kind: "number",
                        required: false,
                        description: "Which link of the text (default 0).",
                    },
                    ParamSchema {
                        name: "dry_run",
                        kind: "boolean",
                        required: false,
                        description: "Answer what would open (and the command) without doing it.",
                    },
                ],
                result: "{ opened: file|asset|entity, path?, line?, col?, command?, action?, guid?, entity? }",
                mutating: true,
                permission: Permission::Play,
            },
            log_open_link_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str) -> LogLink {
        let links = parse_links(text);
        assert_eq!(links.len(), 1, "{text}: {links:?}");
        links.into_iter().next().unwrap_or_else(|| unreachable!())
    }

    #[test]
    fn nothing_that_only_looks_like_a_location() {
        for text in [
            "",
            "listening on 127.0.0.1:8080",
            "connected to example.com:443",
            "at 12:30:45 the frame took 3.5ms",
            "regia_render::shadows:12 did it",
            "see https://example.com/a/b.html:80",
            "localhost:9000",
            "ratio 16:9",
            "a.rs:0",
            "a.rs:12abc",
            "weird a.unknownext:12",
        ] {
            assert_eq!(parse_links(text), Vec::new(), "{text}");
        }
    }

    #[test]
    fn multibyte_text_keeps_offsets_on_char_boundaries() {
        let text = "écoute ▸ src/é.rs:3 ▸ fin";
        let link = one(text);
        assert_eq!(&text[link.start..link.end], "src/é.rs:3");
        // A long line is cut on a boundary.
        let long = "é".repeat(MAX_LINE_LEN);
        assert_eq!(parse_links(&long), Vec::new());
    }

    #[test]
    fn a_pathological_line_stays_bounded() {
        let text = "a.rs:1 ".repeat(5000);
        assert_eq!(parse_links(&text).len(), MAX_LINKS);
        let text = ":1".repeat(20_000);
        assert_eq!(parse_links(&text), Vec::new());
        let text = "/ ".repeat(20_000) + "b.rs:1";
        assert!(parse_links(&text).len() <= 1);
    }

    #[test]
    fn expansion_substitutes_per_word_and_never_splits_a_path() {
        let file = Path::new("/my project/a.rs");
        let none = |_: &str| None;
        assert_eq!(
            expand_command("ed +{line} {file}", file, 7, Some(2), &none),
            ["ed", "+7", "/my project/a.rs"]
        );
        assert_eq!(
            expand_command("code", file, 7, Some(2), &none),
            ["code", "-g", "/my project/a.rs:7:2"]
        );
        assert_eq!(
            expand_command("zed", file, 7, None, &none),
            ["zed", "/my project/a.rs:7:1"]
        );
        assert_eq!(
            expand_command("nvim", file, 7, None, &none),
            ["nvim", "+7", "/my project/a.rs"]
        );
        assert_eq!(
            expand_command(
                "\"/opt/my editor/run\" {file}:{line}:{col}",
                file,
                7,
                Some(2),
                &none
            ),
            ["/opt/my editor/run", "/my project/a.rs:7:2"]
        );
        let env = |name: &str| (name == "EDITOR").then(|| "code --wait".to_owned());
        assert_eq!(
            expand_command("$EDITOR", file, 7, None, &env),
            ["code", "--wait", "-g", "/my project/a.rs:7:1"]
        );
        assert_eq!(
            expand_command("$EDITOR", file, 7, None, &none),
            Vec::<String>::new()
        );
    }

    /// P-154: an entity GUID in a line is a link of its own; GUID-like text inside a longer word is
    /// not; a GUID inside a file link stays part of the file link.
    #[test]
    fn entity_guids_are_links() {
        let text = "spawned 67E55044-10b1-426f-9247-bb680e5fe0c8 at origin";
        let link = one(text);
        assert_eq!(link.target, LinkTarget::Entity);
        assert_eq!(
            &text[link.start..link.end],
            "67E55044-10b1-426f-9247-bb680e5fe0c8"
        );
        assert_eq!(link.path(), "67e55044-10b1-426f-9247-bb680e5fe0c8");
        for text in [
            "x67e55044-10b1-426f-9247-bb680e5fe0c8",
            "67e55044-10b1-426f-9247-bb680e5fe0c8a",
            "67e55044-10b1-426f-9247-bb680e5fe0c",
            "67e5504410b1426f9247bb680e5fe0c8",
            "67e55044-10b1-426f-9247_bb680e5fe0c8",
        ] {
            assert_eq!(parse_links(text), Vec::new(), "{text}");
        }
        let text = "src/67e55044-10b1-426f-9247-bb680e5fe0c8.rs:4 and (11111111-2222-3333-4444-555555555555)";
        let links = parse_links(text);
        assert_eq!(links.len(), 2, "{links:?}");
        assert!(matches!(links[0].target, LinkTarget::File { line: 4, .. }));
        assert_eq!(links[1].target, LinkTarget::Entity);
        assert_eq!(link_json(&links[1])["kind"], "entity");
    }

    /// The click (and `editor.log_open_link`) selects the entity and frames it; an unknown GUID is
    /// not found and leaves the selection alone.
    #[test]
    fn an_entity_link_selects_the_entity() {
        use regia_core::id::Guid;
        use regia_scene::prelude::{EntityGuid, Transform};
        let mut world = World::new();
        let other = world
            .spawn((EntityGuid(Guid::from_u128(1)), Transform::default()))
            .id();
        let target = world
            .spawn((EntityGuid(Guid::from_u128(0xabc)), Transform::default()))
            .id();
        world
            .get_resource_or_insert_with(crate::selection::Selection::default)
            .select(other, crate::selection::SelectMode::Replace);
        let guid = Guid::from_u128(0xabc).to_string();
        let reply = log_open_link_method(
            &mut world,
            &json!({ "text": format!("hit {guid} twice"), "dry_run": true }),
        )
        .expect("dry run");
        assert_eq!(reply["opened"], "entity");
        assert_eq!(
            world.resource::<crate::selection::Selection>().primary(),
            Some(other),
            "a dry run changes nothing"
        );
        let status = open_clicked(&mut world, &one(&format!("hit {guid} twice")));
        assert_eq!(status, format!("Selected entity {guid}"));
        assert_eq!(
            world.resource::<crate::selection::Selection>().entities(),
            [target]
        );
        let missing = Guid::from_u128(0xdead).to_string();
        let err = log_open_link_method(&mut world, &json!({ "text": missing })).expect_err("unknown");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("F0003")
        );
        assert_eq!(
            world.resource::<crate::selection::Selection>().entities(),
            [target]
        );
    }
}
