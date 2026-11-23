// SPDX-License-Identifier: MPL-2.0
//! `.ui.ron` authoring (`specs/18` § 5): a frozen text format describing a UI tree (nodes
//! with classes, style, components, bindings), loaded into the [`UiLibrary`] and spawned like
//! prefabs. Every file starts with `version: N`; files of version 1 to 5 are upgraded by the
//! built-in steps (ADR-1381, ADR-1811, ADR-2045, ADR-2129, ADR-2489), versions newer than
//! [`UI_RON_VERSION`] are rejected, and any other older one needs a registered migrator
//! (`docs/formats/ui-ron.md`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use crate::binding::{Bind, BindConverter, BindMode, BindTarget};
use crate::color::UiColor;
use crate::node::{
    Background, BackgroundKind, Border, Classes, ClipChildren, Disabled, DisabledSubtree, Interactable,
    NodeName, NodeStyle, Opacity, SafeAreaPadding, Shadow, UiImage, UiNode, UiRoot, ZOffset,
};
use crate::scroll::{ItemCount, ListView, ScrollView};
use crate::style::{Dimension, PositionType, Style};
use crate::text::{Text, TextAlign, TextOverflow, TextSpan, TextStyle, WrapMode};
use crate::theme::Themed;
use crate::widgets::{
    Button, Dropdown, Modal, ProgressBar, Slider, TabBar, TextInput, Toggle, Tooltip, VirtualButton,
    VirtualStick,
};

/// The current `.ui.ron` format version (bumps require a migrator). v2 (ADR-1381) adds the menu
/// widgets (`popup_menu`, `context_menu`, `menu_bar`) and their [`MenuEntryDef`] entries; v3
/// (ADR-1811) adds view-model `bindings`, `command`, the bind `mode` and `invert` and five bind
/// targets; v4 (ADR-2045) adds the optional text-style fields `letter_spacing`, `weight`,
/// `underline`, `strikethrough`, `features` and `effects`; v5 (ADR-2129, renumbered from its lane's
/// v4 at the v0.2.13 integration, ADR-2219) adds the optional node field `backdrop_blur`. Each
/// older file means the same at the next version, so the built-in v1 → v2, v2 → v3, v3 → v4 and
/// v4 → v5 steps only rewrite the version number; v6 (ADR-2489) adds the optional node field
/// `repeat`, the optional bind field `converter` and item-relative binding sources (`.field`), so
/// the v5 → v6 step is a renumbering too. v7 (ADR-2866) adds the optional node fields
/// `group_opacity`, `clip_shape` and `mask`; the v6 → v7 step only renumbers. v8 (ADR-3147, spec
/// 111) adds components: the optional top-level `component` declaration and the optional node fields
/// `use`, `params`, `bind_root`, `slots` and `slot`; the v7 → v8 step only renumbers. v9 (ADR-3148)
/// lets an `image` and a `mask` name a picture by asset path (`asset`) or by icon name (`icon`), and
/// gives `image` optional `nine_slice` insets; the v8 → v9 step only renumbers. v10 (ADR-2958, its lane's
/// v6, renumbered at the integ-v049 recovery) adds the game widgets (`lobby_list`, `scoreboard`,
/// `radar`, `marker_layer`); a v9 file means the same, so the v9 → v10 step only renumbers. v11
/// (ADR-9980) gives `text_input` an optional third element of [`TextInputOptions`] (`max_chars`,
/// `secret`, `read_only`, `filter`) and `image` an optional `fit` ([`crate::ImageFit`]); both are
/// optional, so the v10 → v11 step only renumbers. The UI designer's additions fold into v11
/// before it ships (ADR-7751, under ADR-7724's fold rule): the optional bind fields `theme` (a
/// theme token source) and `animation` ([`AnimationBindDef`]), the optional repeat field
/// `row_height` ([`RepeatRowHeight`]) and the image fit `tile`; every earlier v11 file reads
/// unchanged, and the v10 → v11 step still only renumbers. Folded into v11 as well before the
/// v0.6.0 tag (ADR-7827): the `split`, `foldable` and `tabs` containers, the text field `markup`
/// and the node field `nav` (focus neighbors by name); all optional, so every earlier v11 file reads
/// with the same meaning. The same fold adds the optional top-level `variants` (ADR-7858, layout
/// override sets by window orientation and size). Folded into v11 as well (ADR-8252, still one
/// step from the v10 that v0.5.0 shipped): the file field `back` (`ignore` or `pop`), the node
/// field `disabled_subtree`, and the node field `pivot`. All optional, so every earlier v11 file
/// reads with the same meaning.
pub const UI_RON_VERSION: u32 = 11;

/// One parsed `.ui.ron` file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UiFile {
    /// Format version; must equal [`UI_RON_VERSION`] after migration.
    pub version: u32,
    /// Component declaration (v8, ADR-3147): the parameters and slots other documents may set
    /// when they instantiate this one with `use` (`docs/specs/111-ui-components.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<crate::ui_component::ComponentDecl>,
    /// The root node.
    pub root: UiNodeDef,
    /// Layout variants (v11 fold, ADR-7858): override sets chosen by window orientation and size;
    /// the first match applies on top of the base ([`crate::layout_variants`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<crate::layout_variants::LayoutVariant>,
    /// Screen back policy for the spawned root (v11 fold, ADR-8252). Absent leaves the stack
    /// default. `ignore` and `pop` insert [`crate::screens::ScreenBack`] on that root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub back: Option<ScreenBackDef>,
}

/// How a `.ui.ron` screen handles the back action (ADR-8252).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenBackDef {
    /// Leave back handling to game code, even when the stack pops by default.
    #[default]
    Ignore,
    /// Pop this screen on Escape or gamepad B while it owns the active input layer.
    Pop,
}

impl ScreenBackDef {
    /// The runtime component this document field inserts.
    #[must_use]
    pub const fn to_component(self) -> crate::screens::ScreenBack {
        match self {
            Self::Ignore => crate::screens::ScreenBack::Ignore,
            Self::Pop => crate::screens::ScreenBack::Pop,
        }
    }
}

/// The serde field-list fingerprint of the `.ui.ron` format (hashed by `cargo xtask check-formats`).
///
/// The root struct *and* every nested struct are listed, so a shape change anywhere in the tree
/// changes the hash; the enums' variant names (canonical + alias spellings) cover wire renames
/// across authoring, style and text.
pub fn ui_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<UiFile>(),
        regia_core::schema::serde_field_names::<UiNodeDef>(),
        regia_core::schema::serde_field_names::<BorderDef>(),
        regia_core::schema::serde_field_names::<ShadowDef>(),
        regia_core::schema::serde_field_names::<ImageDef>(),
        regia_core::schema::serde_field_names::<TextInputOptions>(),
        regia_core::schema::serde_variant_names::<TextFilterDef>(),
        regia_core::schema::serde_variant_names::<crate::node::ImageFit>(),
        regia_core::schema::serde_field_names::<TextDef>(),
        regia_core::schema::serde_field_names::<BindDef>(),
        regia_core::schema::serde_field_names::<RepeatDef>(),
        regia_core::schema::serde_variant_names::<RepeatRowHeight>(),
        regia_core::schema::serde_field_names::<AnimationBindDef>(),
        regia_core::schema::serde_variant_names::<crate::binding::AnimationRepeat>(),
        regia_core::schema::serde_field_names::<MaskDef>(),
        regia_core::schema::serde_field_names::<crate::ui_component::ComponentDecl>(),
        regia_core::schema::serde_field_names::<crate::ui_component::ParamDecl>(),
        regia_core::schema::serde_field_names::<crate::ui_component::ParamTarget>(),
        regia_core::schema::serde_variant_names::<crate::ui_component::ParamType>(),
        regia_core::schema::serde_variant_names::<crate::ui_component::ParamProperty>(),
        regia_core::schema::serde_variant_names::<crate::ui_component::ParamValue>(),
        regia_core::schema::serde_variant_names::<ClipShapeDef>(),
        regia_core::schema::serde_variant_names::<crate::group_layer::MaskFit>(),
        crate::converter::converter_field_names(),
        regia_core::schema::serde_field_names::<MenuEntryDef>(),
        regia_core::schema::serde_field_names::<LobbyListDef>(),
        regia_core::schema::serde_field_names::<ScoreboardDef>(),
        regia_core::schema::serde_field_names::<ScoreColumnDef>(),
        regia_core::schema::serde_field_names::<RadarDef>(),
        regia_core::schema::serde_field_names::<MarkerLayerDef>(),
        // The v11 containers, markup and focus neighbors fold (ADR-7827).
        regia_core::schema::serde_field_names::<SplitDef>(),
        regia_core::schema::serde_variant_names::<SplitAxisDef>(),
        regia_core::schema::serde_field_names::<FoldableDef>(),
        regia_core::schema::serde_field_names::<TabsDef>(),
        regia_core::schema::serde_field_names::<NavDef>(),
        // Layout variants, same v11 fold (ADR-7858).
        regia_core::schema::serde_field_names::<crate::layout_variants::LayoutVariant>(),
        regia_core::schema::serde_field_names::<crate::layout_variants::VariantWhen>(),
        regia_core::schema::serde_variant_names::<crate::layout_variants::Orientation>(),
        // Screen back, subtree disable and pivot (ADR-8252), folded into v11.
        regia_core::schema::serde_variant_names::<ScreenBackDef>(),
        regia_core::schema::serde_field_names::<Style>(),
        regia_core::schema::serde_field_names::<TextStyle>(),
        regia_core::schema::serde_field_names::<crate::text_effects::TextEffect>(),
        regia_core::schema::serde_variant_names::<BackgroundDef>(),
        regia_core::schema::serde_variant_names::<TextSpanDef>(),
        regia_core::schema::serde_variant_names::<LocSpan>(),
        regia_core::schema::serde_variant_names::<WidgetDef>(),
        regia_core::schema::serde_variant_names::<BindTargetDef>(),
        regia_core::schema::serde_variant_names::<BindMode>(),
        regia_core::schema::serde_variant_names::<BindConverter>(),
        regia_core::schema::serde_variant_names::<crate::style::Display>(),
        regia_core::schema::serde_variant_names::<crate::style::PositionType>(),
        regia_core::schema::serde_variant_names::<crate::style::FlexDirection>(),
        regia_core::schema::serde_variant_names::<crate::style::JustifyContent>(),
        regia_core::schema::serde_variant_names::<crate::style::Align>(),
        regia_core::schema::serde_variant_names::<crate::style::Overflow>(),
        regia_core::schema::serde_variant_names::<crate::style::TrackSizing>(),
        regia_core::schema::serde_variant_names::<crate::style::TextDir>(),
        regia_core::schema::serde_variant_names::<crate::text::TextContent>(),
        regia_core::schema::serde_variant_names::<crate::text::WrapMode>(),
        regia_core::schema::serde_variant_names::<crate::text::TextOverflow>(),
        regia_core::schema::serde_variant_names::<crate::text::TextAlign>(),
    ])
}

/// The `from` versions of the engine's built-in `.ui.ron` migration steps (reported to
/// `cargo xtask check-formats`). Game-registered [`UiMigrators`] steps are not included.
pub fn ui_migration_sources() -> Vec<u32> {
    BUILTIN_UI_STEPS.iter().map(|(from, _)| *from).collect()
}

/// The engine's built-in `.ui.ron` steps as `(from_version, step)`; each step rewrites the source
/// text and returns the version it produced. They run before game-registered steps.
const BUILTIN_UI_STEPS: &[(u32, fn(&mut String) -> Result<u32, String>)] = &[
    (1, ui_v1_to_v2),
    (2, ui_v2_to_v3),
    (3, ui_v3_to_v4),
    (4, ui_v4_to_v5),
    (5, ui_v5_to_v6),
    (6, ui_v6_to_v7),
    (7, ui_v7_to_v8),
    (8, ui_v8_to_v9),
    (9, ui_v9_to_v10),
    (10, ui_v10_to_v11),
];

/// v1 → v2 (ADR-1381): v2 is v1 plus the menu widget variants and `MenuEntryDef`, so a v1 file
/// keeps its meaning and only the root `version` number changes. Every other byte (comments,
/// layout) is kept.
fn ui_v1_to_v2(text: &mut String) -> Result<u32, String> {
    renumber_root_version(text, 2)
}

/// v2 → v3 (ADR-1811): v3 is v2 plus optional fields (`bindings`, `command`, bind `mode` and
/// `invert`) and new bind targets, so a v2 file keeps its meaning; only the number changes.
fn ui_v2_to_v3(text: &mut String) -> Result<u32, String> {
    renumber_root_version(text, 3)
}

/// v3 → v4 (ADR-2045): v4 is v3 plus optional text-style fields (spacing, weight, decorations,
/// OpenType features, effect layers), so a v3 file keeps its meaning; only the number changes.
fn ui_v3_to_v4(text: &mut String) -> Result<u32, String> {
    renumber_root_version(text, 4)
}

/// v4 → v5 (ADR-2129): v5 is v4 plus the optional `backdrop_blur` node field, so a v4 file keeps
/// its meaning; only the number changes.
fn ui_v4_to_v5(text: &mut String) -> Result<u32, String> {
    renumber_root_version(text, 5)
}

/// v5 → v6 (ADR-2489): v6 is v5 plus the optional `repeat` node field, the optional `converter` bind
/// field and sources starting with a dot, which no v5 file means anything by (a v5 `.x` source never
/// resolved), so a v5 file keeps its meaning; only the number changes.
fn ui_v5_to_v6(text: &mut String) -> Result<u32, String> {
    renumber_root_version(text, 6)
}

/// v6 → v7 (ADR-2866): v7 is v6 plus the optional `group_opacity`, `clip_shape` and `mask` node
/// fields, so a v6 file keeps its meaning; only the number changes.
fn ui_v6_to_v7(text: &mut String) -> Result<u32, String> {
    renumber_root_version(text, 7)
}

/// v7 → v8 (ADR-3147): v8 is v7 plus the optional top-level `component` field and the optional
/// node fields `use`, `params`, `bind_root`, `slots` and `slot`, so a v7 file keeps its meaning;
/// only the number changes.
fn ui_v7_to_v8(text: &mut String) -> Result<u32, String> {
    renumber_root_version(text, 8)
}

/// v8 → v9 (ADR-3148): v9 is v8 plus the optional `asset`, `icon` and `nine_slice` image fields and
/// the optional `icon` mask field; a v8 `image` always named a renderer id, which v9 reads the same
/// way, so a v8 file keeps its meaning; only the number changes.
fn ui_v8_to_v9(text: &mut String) -> Result<u32, String> {
    renumber_root_version(text, 9)
}

/// A node's style as authored, with the size a game widget needs when none is given: everything a
/// marker layer or a radar draws is positioned by hand, so with an automatic size it would lay out
/// at zero and show nothing. A marker layer fills its parent, a radar is 200 px square, whether
/// the node flows or is anchored by hand (`position: Absolute`); an absolute node keeps its own
/// insets, and an axis stretched between two insets keeps its automatic size.
fn authored_style(def: &UiNodeDef) -> Style {
    let mut style = def.style.clone();
    if style.size != [Dimension::Auto; 2] {
        return style;
    }
    // The size applies whenever none is given; only a flowing marker layer also becomes an
    // overlay at the parent's top-left (an absolute one keeps the anchor its author gave it).
    let size = match def.widget {
        Some(WidgetDef::MarkerLayer(_)) => {
            if style.position != PositionType::Absolute {
                style.position = PositionType::Absolute;
                style.inset.left = Dimension::Points(0.0);
                style.inset.top = Dimension::Points(0.0);
            }
            Dimension::Percent(100.0)
        }
        Some(WidgetDef::Radar(_)) => Dimension::Points(200.0),
        _ => return style,
    };
    // An absolute node with both insets of an axis set is stretched between them on that axis.
    let stretched = |a: Dimension, b: Dimension| {
        style.position == PositionType::Absolute && a != Dimension::Auto && b != Dimension::Auto
    };
    let by_insets = [
        stretched(style.inset.left, style.inset.right),
        stretched(style.inset.top, style.inset.bottom),
    ];
    for (axis, by_insets) in by_insets.into_iter().enumerate() {
        if !by_insets {
            style.size[axis] = size;
        }
    }
    style
}

/// v9 → v10 (ADR-2958): v10 is v9 plus the game widget variants, so a v9 file keeps its meaning;
/// only the number changes.
fn ui_v9_to_v10(text: &mut String) -> Result<u32, String> {
    renumber_root_version(text, 10)
}

/// v10 → v11 (ADR-9980): v11 is v10 plus the optional `text_input` options element and the optional
/// image `fit` (absent, `preserve_aspect` decides as before), so a v10 file keeps its meaning; only
/// the number changes.
fn ui_v10_to_v11(text: &mut String) -> Result<u32, String> {
    renumber_root_version(text, 11)
}

/// Rewrites the root `version` number to `to`, keeping every other byte.
fn renumber_root_version(text: &mut String, to: u32) -> Result<u32, String> {
    let (start, end) =
        root_version_span(text).ok_or_else(|| "no root `version: <number>` field".to_owned())?;
    text.replace_range(start..end, &to.to_string());
    Ok(to)
}

/// Byte range of the number after the root struct's `version:` key, `None` when the root has no
/// such field. Strings, raw strings, char literals and (nested) comments are skipped, and only
/// keys at the root struct's own depth count, so a `version` inside a node, a string or a comment
/// is never touched.
fn root_version_span(text: &str) -> Option<(usize, usize)> {
    let b = text.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => i = skip_quoted(b, i, b'"'),
            b'\'' => i = skip_quoted(b, i, b'\''),
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => i = skip_block_comment(b, i),
            // An inner attribute before the root value (`#![enable(implicit_some)]`).
            b'#' if depth == 0 => {
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
                i += 1;
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' | b'}' => {
                if depth <= 1 {
                    return None;
                }
                depth -= 1;
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                let ident = &b[start..i];
                if (ident == b"r" || ident == b"br") && matches!(b.get(i), Some(b'"' | b'#')) {
                    i = skip_raw_string(b, i);
                } else if depth == 1 && ident == b"version" {
                    let colon = skip_trivia(b, i);
                    if b.get(colon) != Some(&b':') {
                        continue;
                    }
                    let num = skip_trivia(b, colon + 1);
                    let mut end = num;
                    while end < b.len()
                        && (b[end].is_ascii_alphanumeric() || b[end] == b'_' || b[end] == b'+')
                    {
                        end += 1;
                    }
                    return (end > num).then_some((num, end));
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Index just past the quoted literal opening at `open` (`"…"` or `'…'`, backslash escapes).
fn skip_quoted(b: &[u8], open: usize, quote: u8) -> usize {
    let mut i = open + 1;
    while i < b.len() && b[i] != quote {
        i += if b[i] == b'\\' { 2 } else { 1 };
    }
    i + 1
}

/// Index just past the raw string whose `#`s or opening quote start at `i` (`r#"…"#`).
fn skip_raw_string(b: &[u8], mut i: usize) -> usize {
    let mut hashes = 0;
    while b.get(i) == Some(&b'#') {
        hashes += 1;
        i += 1;
    }
    if b.get(i) != Some(&b'"') {
        return i;
    }
    i += 1;
    while i < b.len() {
        if b[i] == b'"'
            && b.get(i + 1..i + 1 + hashes)
                .is_some_and(|tail| tail.iter().all(|&c| c == b'#'))
        {
            return i + 1 + hashes;
        }
        i += 1;
    }
    i
}

/// Index just past the (possibly nested) `/* */` comment opening at `open`.
fn skip_block_comment(b: &[u8], open: usize) -> usize {
    let mut nest = 1;
    let mut i = open + 2;
    while i < b.len() && nest > 0 {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            nest += 1;
            i += 2;
        } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
            nest -= 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    i
}

/// Index of the first byte at or after `i` that is not whitespace or a comment.
fn skip_trivia(b: &[u8], mut i: usize) -> usize {
    loop {
        match b.get(i) {
            Some(c) if c.is_ascii_whitespace() => i += 1,
            Some(b'/') if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            Some(b'/') if b.get(i + 1) == Some(&b'*') => i = skip_block_comment(b, i),
            _ => return i,
        }
    }
}

/// One node definition.
///
/// Serialization (the [`write_ui_ron`] writer) omits every field that holds its default, so a
/// written file lists only what was set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct UiNodeDef {
    /// Optional node name (debugging, bindings).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Theme style classes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub classes: Vec<String>,
    /// Layout style.
    #[serde(
        skip_serializing_if = "is_default_style",
        serialize_with = "sparse_style::serialize"
    )]
    pub style: Style,
    /// Background fill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<BackgroundDef>,
    /// Border stroke.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border: Option<BorderDef>,
    /// Drop shadow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow: Option<ShadowDef>,
    /// Image fill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageDef>,
    /// Text content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextDef>,
    /// Opacity multiplier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Frosted glass (v4, ADR-2129): blurs what is drawn behind the node, Gaussian sigma in UI px
    /// ([`crate::BackdropBlur`]). Off when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backdrop_blur: Option<f32>,
    /// Clip children to the rect.
    #[serde(skip_serializing_if = "is_false")]
    pub clip: bool,
    /// Receives pointer events.
    #[serde(skip_serializing_if = "is_false")]
    pub interactable: bool,
    /// Starts disabled.
    #[serde(skip_serializing_if = "is_false")]
    pub disabled: bool,
    /// Disables pointer and focus input for this node and every descendant (v11 fold, ADR-8252).
    /// A local `disabled` flag does not. The document root ignores node fields other than style
    /// and children; put this on a child.
    #[serde(skip_serializing_if = "is_false")]
    pub disabled_subtree: bool,
    /// Transform pivot as a fraction of the node's layout rect (v11 fold, ADR-8252). Absent keeps
    /// the centre (`[0.5, 0.5]`). Any finite ratio is kept, including one outside 0..=1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pivot: Option<[f32; 2]>,
    /// Draw-order offset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub z: Option<i32>,
    /// Respect the platform safe area (roots only).
    #[serde(skip_serializing_if = "is_false")]
    pub safe_area: bool,
    /// Widget components.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub widget: Option<WidgetDef>,
    /// Data binding (Tier 1, one per node).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<BindDef>,
    /// View-model bindings (v3, ADR-1811): any number per node, spawned as
    /// [`crate::viewmodel::ViewBindings`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<BindDef>,
    /// Command sent as a [`crate::viewmodel::UiCommand`] on click or submit (v3, ADR-1811).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Collection item template (v6, ADR-2489): `children` spawn once per element of the list at
    /// `source` instead of once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat: Option<RepeatDef>,
    /// Isolated group opacity (ADR-2866, optional; added in `.ui.ron` v7): the node and its subtree
    /// composite once at this opacity ([`crate::GroupOpacity`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_opacity: Option<f32>,
    /// Clip shape (ADR-2866, optional): `rect` is `clip: true`; `rounded` clips the children to
    /// the node's rounded rect with an antialiased edge ([`crate::RoundedClip`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip_shape: Option<ClipShapeDef>,
    /// Image alpha mask (ADR-2866, optional): `(image: 7, fit: contain)` ([`crate::UiMask`]) or
    /// `(asset: "assets/ui/star.png")` ([`crate::UiMaskAsset`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask: Option<MaskDef>,
    /// Component instance (v8, ADR-3147): the path under `assets/` of the `.ui.ron` this node
    /// instantiates (`use: "ui/health_bar.ui.ron"`). Expanded by [`crate::ui_component::expand`].
    #[serde(rename = "use", skip_serializing_if = "Option::is_none")]
    pub use_component: Option<String>,
    /// Instance parameter values (v8): declared parameters of the used component only.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, crate::ui_component::ParamValue>,
    /// Instance view-model root (v8): the component's `$` sources resolve against it (`"Party.p1"`,
    /// or `"."` for the item of an enclosing repeat).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_root: Option<String>,
    /// Instance slot content by slot name (v8); plain `children` fill the `default` slot.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub slots: BTreeMap<String, Vec<UiNodeDef>>,
    /// Slot outlet (v8, component documents): instance content for this slot replaces the node's
    /// children.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    /// Declared focus neighbors by node name (v11 fold, ADR-7827).
    #[serde(skip_serializing_if = "NavDef::is_empty")]
    pub nav: NavDef,
    /// Children (with `repeat`, the item template).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<UiNodeDef>,
}

/// A node's clip shape (ADR-2866).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipShapeDef {
    /// Clip children to the node's rect (as `clip: true`).
    Rect,
    /// Clip children to the node's rounded rect (its border or theme radius), antialiased.
    Rounded,
}

/// A node's image alpha mask (ADR-2866): exactly one of `image`, `asset` and `icon` (v9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaskDef {
    /// Renderer image id whose alpha masks the node (as `image: (id: ...)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<u64>,
    /// Manifest path of a cooked image (`assets/ui/star.png`) whose alpha masks the node
    /// ([`crate::UiMaskAsset`]), loaded in the background (ADR-3148).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    /// v9 (ADR-3148): name of an icon in the [`crate::icons::IconAtlas`] whose alpha masks the node
    /// ([`crate::UiMaskIcon`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Placement: `stretch` (default), `contain`, `cover` or `tile`.
    #[serde(default, skip_serializing_if = "is_default_value")]
    pub fit: crate::group_layer::MaskFit,
}

/// A node's collection binding (v6, ADR-2489): `repeat: (source: "InventoryVm.items", key: "id")`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RepeatDef {
    /// The list: `"Model.field[.field…]"` of a reflected resource, or `".field"` relative to the
    /// item of an enclosing repeat.
    pub source: String,
    /// The path inside an item of its stable key (`"id"`, `"."` for the item itself). Without one
    /// items are keyed by position, so a removal in the middle rewrites the rows after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Row height of a virtualized repeat (a `list` widget host; v11 fold, ADR-7751): `list`
    /// (default) keeps the list's `row_height`, `template` takes the fixed height of the template's
    /// first node ([`RepeatDef::template_row_height`]). Ignored on a repeat that is not a list.
    #[serde(default, skip_serializing_if = "is_default_value")]
    pub row_height: RepeatRowHeight,
}

/// Where a virtualized repeat's rows get their height (ADR-7751).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatRowHeight {
    /// The list widget's own `row_height` (or `ListHeights`).
    #[default]
    List,
    /// The fixed height (`style.size` height in UI px) of the template's first node; a template
    /// without one keeps the list's height and the designer lints `repeat_template_height`.
    Template,
}

impl RepeatRowHeight {
    /// The `.ui.ron` spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Template => "template",
        }
    }
}

impl RepeatDef {
    /// The row height `template` gives a virtualized repeat with `row_height: template`: the first
    /// template node's fixed, finite, positive height in UI px; `None` when it has none.
    pub fn template_row_height(template: &[UiNodeDef]) -> Option<f32> {
        match template.first()?.style.size[1] {
            Dimension::Points(h) if h.is_finite() && h > 0.0 => Some(h),
            _ => None,
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_true(b: &bool) -> bool {
    *b
}

fn is_white(c: &UiColor) -> bool {
    *c == UiColor::WHITE
}

fn is_default_line_height(h: &f32) -> bool {
    *h == default_line_height()
}

fn is_default_value<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

fn is_default_style(s: &Style) -> bool {
    *s == Style::default()
}

/// Background definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BackgroundDef {
    /// Solid colour.
    Color(UiColor),
    /// Gradient (two stops and an angle in degrees).
    Gradient {
        /// Start colour.
        from: UiColor,
        /// End colour.
        to: UiColor,
        /// Angle in degrees (0 = top→bottom).
        angle: f32,
    },
}

/// Border definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BorderDef {
    /// Border colour.
    pub color: UiColor,
    /// Uniform width (UI px).
    pub width: f32,
    /// Uniform corner radius (UI px).
    pub radius: f32,
}

/// Shadow definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShadowDef {
    /// Shadow colour.
    pub color: UiColor,
    /// Blur radius (UI px).
    pub blur: f32,
    /// Offset (x, y) in UI px.
    pub offset: (f32, f32),
}

/// Image definition: the picture is named by exactly one of a renderer image `id` (code that
/// registers images at run time), an asset path (`asset`, v9) or an icon name (`icon`, v9). With
/// `asset` or `icon` the id stays 0 and is not written (ADR-3148, [`crate::UiImageAsset`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageDef {
    /// Renderer image id (0 when `asset` or `icon` names the picture).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub id: u64,
    /// v9: manifest path of a cooked image or texture (`"assets/ui/panel.png"`), loaded through the
    /// asset server in the background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    /// v9: name of an icon in the [`crate::icons::IconAtlas`] (without `@size`); the drawing whose size
    /// best fits the node's laid-out height is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Tint colour.
    #[serde(default = "white_default", skip_serializing_if = "is_white")]
    pub tint: UiColor,
    /// Keep aspect ratio (without `fit`: `true` letterboxes, as `fit: contain`; `false` stretches).
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub preserve_aspect: bool,
    /// v11 (ADR-9980): how the picture fills the node (`stretch`, `contain`, `cover`); wins over
    /// `preserve_aspect` when both are written. See [`ImageDef::effective_fit`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<crate::node::ImageFit>,
    /// v9: nine-slice insets `(left, top, right, bottom)` in source pixels: the corners keep their
    /// size and the edges and middle stretch. The image then fills the node as its background
    /// ([`crate::BackgroundKind::Image`], replacing a `background`), and `tint` and
    /// `preserve_aspect` do not apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nine_slice: Option<(f32, f32, f32, f32)>,
}

impl Default for ImageDef {
    fn default() -> Self {
        Self {
            id: 0,
            asset: None,
            icon: None,
            tint: UiColor::WHITE,
            preserve_aspect: true,
            fit: None,
            nine_slice: None,
        }
    }
}

impl ImageDef {
    /// How the picture fills the node: `fit` when written, else `contain` for `preserve_aspect`
    /// (the default) and `stretch` without it (v10 and older files).
    #[must_use]
    pub fn effective_fit(&self) -> crate::node::ImageFit {
        use crate::node::ImageFit;
        match self.fit {
            Some(fit) => fit,
            None if self.preserve_aspect => ImageFit::Contain,
            None => ImageFit::Stretch,
        }
    }

    /// The image registered with the renderer under `id`.
    #[must_use]
    pub fn raw(id: u64) -> Self {
        Self {
            id,
            ..Self::default()
        }
    }

    /// The cooked image at the manifest path `path`.
    #[must_use]
    pub fn asset(path: impl Into<String>) -> Self {
        Self {
            asset: Some(path.into()),
            ..Self::default()
        }
    }

    /// The icon `name` of the [`crate::icons::IconAtlas`].
    #[must_use]
    pub fn icon(name: impl Into<String>) -> Self {
        Self {
            icon: Some(name.into()),
            ..Self::default()
        }
    }

    /// The picture this image names: `None` for a raw renderer id.
    #[must_use]
    pub fn source(&self) -> Option<crate::ui_image_assets::UiImageSource> {
        use crate::ui_image_assets::UiImageSource;
        match (&self.asset, &self.icon) {
            (Some(path), _) => Some(UiImageSource::Asset(path.clone())),
            (None, Some(name)) => Some(UiImageSource::Icon(name.clone())),
            (None, None) => None,
        }
    }
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// Default tint (white).
fn white_default() -> UiColor {
    UiColor::WHITE
}

/// Text definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextDef {
    /// Literal text or localization key list.
    pub spans: Vec<TextSpanDef>,
    /// Alignment.
    #[serde(default, skip_serializing_if = "is_default_value")]
    pub align: TextAlign,
    /// Wrapping.
    #[serde(default, skip_serializing_if = "is_default_value")]
    pub wrap: WrapMode,
    /// Overflow behaviour.
    #[serde(default, skip_serializing_if = "is_default_value")]
    pub overflow: TextOverflow,
    /// Line-height multiplier.
    #[serde(
        default = "default_line_height",
        skip_serializing_if = "is_default_line_height"
    )]
    pub line_height: f32,
    /// Base text style.
    #[serde(default, skip_serializing_if = "is_default_value")]
    pub style: TextStyle,
    /// Rich-text markup (v11 fold, ADR-7827): literal spans are parsed by
    /// [`crate::text::parse_markup`] (`<b>`, `<i>`, `<u>`, `<s>`, `<color=#..>`, `<size=..>`,
    /// `<icon=name>`); off by default, so `<` is plain text.
    #[serde(default, skip_serializing_if = "is_false")]
    pub markup: bool,
}

impl TextDef {
    /// One literal span with the default alignment, wrapping, overflow, line height and style.
    pub fn literal(text: impl Into<String>) -> Self {
        Self {
            spans: vec![TextSpanDef::Lit(text.into())],
            align: TextAlign::default(),
            wrap: WrapMode::default(),
            overflow: TextOverflow::default(),
            line_height: default_line_height(),
            style: TextStyle::default(),
            markup: false,
        }
    }
}

/// One text span.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TextSpanDef {
    /// Literal text (`"Score: 0"`).
    Lit(String),
    /// Externally tagged form (`Loc("menu-start")`).
    Loc(LocSpan),
    /// Full definition (`(text: "...", style: (...))`).
    Full {
        /// Literal or `loc:key`.
        text: String,
        /// Style overrides.
        #[serde(default, skip_serializing_if = "is_default_value")]
        style: TextStyle,
    },
}

/// Externally tagged localization span: `Loc("key")`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LocSpan {
    /// Localization key.
    Loc(String),
}

/// Widget component(s) of a node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetDef {
    /// Push button.
    Button,
    /// Toggle switch (`checked` initial state).
    Toggle(bool),
    /// Slider (value, min, max, step).
    Slider(f32, f32, f32, f32),
    /// Progress bar (initial value).
    Progress(f32),
    /// Text input (placeholder, multiline, options). The options element is optional in the file
    /// (v11, ADR-9980): `text_input("Name", false)` reads with every option at its default, and the
    /// writer leaves it out while every option is default.
    TextInput(
        String,
        bool,
        #[serde(default, skip_serializing_if = "TextInputOptions::is_default")] TextInputOptions,
    ),
    /// Dropdown (options, selected index).
    Dropdown(Vec<String>, Option<usize>),
    /// Tab bar (tabs, active index).
    TabBar(Vec<String>, usize),
    /// Modal dialog (open, close on cancel).
    Modal(bool, bool),
    /// Tooltip (text, delay seconds).
    Tooltip(String, f32),
    /// Scroll view (inertia seconds, scrollbar).
    Scroll(f32, bool),
    /// Virtualised list (item count, row height).
    List(usize, f32),
    /// Focusable for keyboard/gamepad navigation.
    Focusable,
    /// Virtual thumbstick (action, radius).
    Stick(String, f32),
    /// Virtual button (action).
    VButton(String),
    /// Popup menu opened by clicking the node (entries).
    PopupMenu(Vec<MenuEntryDef>),
    /// Context menu opened by a secondary click or long press on the node (entries).
    ContextMenu(Vec<MenuEntryDef>),
    /// Menu bar: one title per entry, the entry's `submenu` is its menu.
    MenuBar(Vec<MenuEntryDef>),
    /// Lobby list (v10): the lobbies `lobby.*` lists, or the current lobby's members.
    LobbyList(LobbyListDef),
    /// Scoreboard (v10): the game rules' score table.
    Scoreboard(ScoreboardDef),
    /// Radar or minimap (v10): every `RadarBlip` entity.
    Radar(RadarDef),
    /// Overlay of world markers and target brackets (v10): every `WorldMarker` and `TargetLock`
    /// entity, through the camera.
    MarkerLayer(MarkerLayerDef),
    /// Split container (v11 fold, ADR-7827): the node's first two children are two panes with a
    /// draggable divider between them ([`crate::Split`]).
    Split(SplitDef),
    /// Foldable section (v11 fold, ADR-7827): a titled header that shows or hides the node's
    /// children; foldables of one `group` under one parent are an accordion ([`crate::Foldable`]).
    Foldable(FoldableDef),
    /// Tab container (v11 fold, ADR-7827): a tab bar above the node's children, child `i` being the
    /// page of tab `i`; only the active page shows ([`crate::TabPages`]).
    Tabs(TabsDef),
}

/// Which way a `split` lays its panes in `.ui.ron` (v11 fold, ADR-7827).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxisDef {
    /// Panes side by side (the default).
    #[default]
    Horizontal,
    /// Panes stacked.
    Vertical,
}

impl SplitAxisDef {
    /// The runtime axis.
    #[must_use]
    pub fn to_axis(self) -> crate::default_widgets::SplitAxis {
        match self {
            Self::Horizontal => crate::default_widgets::SplitAxis::Horizontal,
            Self::Vertical => crate::default_widgets::SplitAxis::Vertical,
        }
    }
}

/// A `split` in `.ui.ron` (v11 fold, ADR-7827): `split((axis: vertical, ratio: 0.3))`. Every field is
/// optional; the writer leaves out the defaults (horizontal, 0.5, 0.1, 0.9).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SplitDef {
    /// Pane layout.
    #[serde(skip_serializing_if = "is_default_value")]
    pub axis: SplitAxisDef,
    /// Share of the first pane.
    #[serde(skip_serializing_if = "is_split_ratio_default")]
    pub ratio: f32,
    /// Smallest share of the first pane (its minimum size).
    #[serde(skip_serializing_if = "is_split_min_default")]
    pub min_ratio: f32,
    /// Largest share of the first pane (`1 - max_ratio` is the second pane's minimum size).
    #[serde(skip_serializing_if = "is_split_max_default")]
    pub max_ratio: f32,
}

impl Default for SplitDef {
    fn default() -> Self {
        Self {
            axis: SplitAxisDef::Horizontal,
            ratio: 0.5,
            min_ratio: 0.1,
            max_ratio: 0.9,
        }
    }
}

impl SplitDef {
    /// The runtime component (the ratio clamped to the bounds).
    #[must_use]
    pub fn to_split(&self) -> crate::default_widgets::Split {
        let split = crate::default_widgets::Split {
            axis: self.axis.to_axis(),
            ratio: self.ratio,
            min_ratio: self.min_ratio,
            max_ratio: self.max_ratio,
        };
        let ratio = split.clamp_ratio(if self.ratio.is_finite() { self.ratio } else { 0.5 });
        crate::default_widgets::Split {
            ratio: if ratio.is_finite() { ratio } else { 0.5 },
            ..split
        }
    }
}

fn is_split_ratio_default(v: &f32) -> bool {
    *v == 0.5
}

fn is_split_min_default(v: &f32) -> bool {
    *v == 0.1
}

fn is_split_max_default(v: &f32) -> bool {
    *v == 0.9
}

/// A `foldable` in `.ui.ron` (v11 fold, ADR-7827): `foldable((title: "Advanced", open: true, group:
/// "settings"))`. The node's children are the body.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FoldableDef {
    /// Header text (a `loc:` prefix makes it a localization key).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Whether the body starts shown.
    #[serde(skip_serializing_if = "is_false")]
    pub open: bool,
    /// Accordion group: opening one foldable of the group closes its siblings of the same group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// A `tabs` container in `.ui.ron` (v11 fold, ADR-7827): `tabs((tabs: ["General", "Audio"], active:
/// 1))`. Child `i` of the node is the page of tab `i`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TabsDef {
    /// Tab labels, one per page.
    pub tabs: Vec<String>,
    /// Active tab index.
    #[serde(skip_serializing_if = "is_zero_usize")]
    pub active: usize,
}

/// A node's declared focus neighbors in `.ui.ron` (v11 fold, ADR-7827): `nav: (down: "quit", right:
/// "volume")`. Each names another node of the same document by `name`; the runtime writes the
/// resolved entities into the node's [`crate::NavLinks`]. A neighbor wins over the geometric
/// search only while its target is navigable (focusable, enabled, shown).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NavDef {
    /// Neighbor when navigating up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up: Option<String>,
    /// Neighbor when navigating down.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub down: Option<String>,
    /// Neighbor when navigating left.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<String>,
    /// Neighbor when navigating right.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right: Option<String>,
}

impl NavDef {
    /// Whether no neighbor is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The four directions with their declared target names, in `up, down, left, right` order.
    #[must_use]
    pub fn entries(&self) -> [(&'static str, Option<&str>); 4] {
        [
            ("up", self.up.as_deref()),
            ("down", self.down.as_deref()),
            ("left", self.left.as_deref()),
            ("right", self.right.as_deref()),
        ]
    }

    /// The slot of direction `dir` (`up`, `down`, `left`, `right`).
    pub fn slot_mut(&mut self, dir: &str) -> Option<&mut Option<String>> {
        match dir {
            "up" => Some(&mut self.up),
            "down" => Some(&mut self.down),
            "left" => Some(&mut self.left),
            "right" => Some(&mut self.right),
            _ => None,
        }
    }
}

/// The options of a `text_input` in `.ui.ron` (v11, ADR-9980): the third element,
/// `text_input("PIN", false, (max_chars: 4, secret: '*', filter: integer))`. Every field is optional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct TextInputOptions {
    /// Most characters the field takes (Unicode scalar values, whole grapheme clusters); 0 = no
    /// limit.
    #[serde(skip_serializing_if = "is_zero_usize")]
    pub max_chars: usize,
    /// The mask character a password field draws instead of its text (masked text is never
    /// copied or cut).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<char>,
    /// The content can be selected but not changed.
    #[serde(skip_serializing_if = "is_false")]
    pub read_only: bool,
    /// Which edits are accepted.
    #[serde(skip_serializing_if = "is_default_value")]
    pub filter: TextFilterDef,
}

impl TextInputOptions {
    /// Whether every option is at its default (the writer then leaves the element out).
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn is_zero_usize(v: &usize) -> bool {
    *v == 0
}

/// A text input's named filter in `.ui.ron` (v11): [`crate::InputFilter`] without the code-only
/// custom function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TextFilterDef {
    /// Any text.
    #[default]
    Any,
    /// An optionally signed whole number.
    Integer,
    /// An optionally signed decimal number.
    Numeric,
}

impl TextFilterDef {
    /// The runtime filter.
    #[must_use]
    pub fn to_filter(self) -> crate::InputFilter {
        match self {
            Self::Any => crate::InputFilter::Any,
            Self::Integer => crate::InputFilter::Integer,
            Self::Numeric => crate::InputFilter::Numeric,
        }
    }
}

/// A lobby list in `.ui.ron` (`lobby_list`, v10). Empty text keeps the default word; a word may be
/// `"loc:<key>"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct LobbyListDef {
    /// Heading.
    pub title: String,
    /// The refresh button's word.
    pub refresh: String,
    /// The create button's word.
    pub create: String,
    /// Shown while no lobby is listed.
    pub empty: String,
    /// The ready button's word.
    pub ready: String,
    /// The leave button's word.
    pub leave: String,
    /// The owner's launch button's word.
    pub launch: String,
}

/// One column of a [`ScoreboardDef`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ScoreColumnDef {
    /// Stable key (the rules' column keys match it).
    pub key: String,
    /// Heading; empty uses the key.
    pub title: String,
    /// Width in UI px; `0` uses the default.
    pub width: f32,
}

/// A scoreboard in `.ui.ron` (`scoreboard`, v10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScoreboardDef {
    /// Heading; may be empty or `"loc:<key>"`.
    pub title: String,
    /// Columns shown until the game's data names its own.
    pub columns: Vec<ScoreColumnDef>,
    /// The column it starts sorted by, by index; `None` sorts by name.
    pub sort_column: Option<usize>,
    /// Largest first.
    pub descending: bool,
    /// Draw team blocks.
    pub group_by_team: bool,
    /// Show only while this input action is held; empty always shows it.
    pub hold_action: String,
    /// Take the data from the game rules (`false`: the game writes it).
    pub from_rules: bool,
}

impl Default for ScoreboardDef {
    fn default() -> Self {
        Self {
            title: String::new(),
            columns: Vec::new(),
            sort_column: Some(0),
            descending: true,
            group_by_team: true,
            hold_action: String::new(),
            from_rules: true,
        }
    }
}

/// A radar or minimap in `.ui.ron` (`radar`, v10). A `world_rect` makes it a minimap of that area;
/// without one it is a radar of `range` metres.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RadarDef {
    /// Metres from the center to the edge (a radar).
    pub range: f32,
    /// `[min_x, min_z, max_x, max_z]` in metres: makes it a minimap of that area.
    pub world_rect: Option<[f32; 4]>,
    /// A square outline; `false` is a circle.
    pub square: bool,
    /// Turn with the player's heading (a radar without a map).
    pub rotate: bool,
    /// Image id of a map behind the blips; `0` draws none.
    pub map_image: u64,
    /// The area the map image covers; `None` for a minimap means its `world_rect`.
    pub map_rect: Option<[f32; 4]>,
    /// Which blip layers show (bit mask).
    pub layer_mask: u32,
    /// Most blips shown; the nearest win.
    pub max_blips: u32,
    /// Draw the "N" tick.
    pub show_north: bool,
}

impl Default for RadarDef {
    fn default() -> Self {
        Self {
            range: 50.0,
            world_rect: None,
            square: false,
            rotate: false,
            map_image: 0,
            map_rect: None,
            layer_mask: u32::MAX,
            max_blips: crate::game_widgets::radar::DEFAULT_MAX_BLIPS as u32,
            show_north: true,
        }
    }
}

impl RadarDef {
    /// The component this describes.
    #[must_use]
    pub fn to_radar(&self) -> crate::game_widgets::Radar {
        use crate::game_widgets::{Radar, RadarMap, RadarMode, RadarShape};
        let mode = match self.world_rect {
            Some(world_rect) => RadarMode::Minimap { world_rect },
            None => RadarMode::Radar { range: self.range },
        };
        let shape = if self.square || self.world_rect.is_some() {
            RadarShape::Rect
        } else {
            RadarShape::Circle
        };
        Radar {
            mode,
            shape,
            rotate: self.rotate,
            layer_mask: self.layer_mask,
            max_blips: self.max_blips as usize,
            map: (self.map_image != 0).then(|| RadarMap {
                image: self.map_image,
                world_rect: self.map_rect.or(self.world_rect).unwrap_or([0.0; 4]),
            }),
            show_north: self.show_north,
            ..Radar::default()
        }
    }
}

/// An overlay of world markers and target brackets in `.ui.ron` (`marker_layer`, v10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarkerLayerDef {
    /// Which marker layers show (bit mask).
    pub layer_mask: u32,
    /// Distance kept from the edge by pinned markers, in UI px.
    pub edge_margin: f32,
    /// Most markers shown; the nearest win.
    pub max_markers: u32,
}

impl Default for MarkerLayerDef {
    fn default() -> Self {
        let layer = crate::game_widgets::MarkerLayer::default();
        Self {
            layer_mask: layer.layer_mask,
            edge_margin: layer.edge_margin,
            max_markers: layer.max_markers as u32,
        }
    }
}

/// Binding definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BindDef {
    /// Source path: `"Model.field[.field…]"` of a reflected resource (`"Score.value"`), or in a
    /// repeat template `".field[.field…]"` of the item (`"."` is the item itself; v6). Empty (and
    /// not written) on a `theme` or `animation` binding (v11 fold, ADR-7751).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// Target property.
    #[serde(default, skip_serializing_if = "is_default_target")]
    pub target: BindTargetDef,
    /// Format (`"{:03}"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Direction (v3, ADR-1811): `one_way` (default), `two_way` or `one_time`.
    #[serde(default, skip_serializing_if = "is_default_mode")]
    pub mode: BindMode,
    /// Negate a boolean before it reaches `checked`, `visible` or `enabled` (v3).
    #[serde(default, skip_serializing_if = "is_false")]
    pub invert: bool,
    /// Transforms the value before `format` and `invert` (v6, ADR-2489).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converter: Option<BindConverter>,
    /// v11 fold (ADR-7751): read this token of the active theme (`"color.accent"`) instead of
    /// `source`; the value follows a theme switch ([`crate::BindSource::Theme`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// v11 fold (ADR-7751): push a number animated over time instead of reading `source`
    /// ([`crate::BindSource::Animation`]). `theme` wins when both are written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub animation: Option<AnimationBindDef>,
}

/// An animation binding (v11 fold, ADR-7751): `animation: (from: 0, to: 1, duration: 0.8,
/// ease: "quad_in_out", repeat: ping_pong)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnimationBindDef {
    /// Value at the start of a pass.
    pub from: f32,
    /// Value at the end of a pass.
    pub to: f32,
    /// Seconds per pass (zero or less jumps to `to`).
    pub duration: f32,
    /// Seconds before the first pass.
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub delay: f32,
    /// Easing name (`crate::tween::Ease::parse`, e.g. `"quad_in_out"`); linear when absent or
    /// unknown (the designer lints `binding_bad_animation`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ease: Option<String>,
    /// `once` (default), `loop` or `ping_pong`.
    #[serde(default, skip_serializing_if = "is_default_value")]
    pub repeat: crate::binding::AnimationRepeat,
}

impl Default for AnimationBindDef {
    fn default() -> Self {
        Self {
            from: 0.0,
            to: 1.0,
            duration: 1.0,
            delay: 0.0,
            ease: None,
            repeat: crate::binding::AnimationRepeat::Once,
        }
    }
}

impl AnimationBindDef {
    /// The runtime animation (an unknown ease name reads as linear).
    pub fn to_animation(&self) -> crate::binding::BindAnimation {
        crate::binding::BindAnimation {
            from: self.from,
            to: self.to,
            duration: self.duration,
            delay: self.delay,
            ease: self
                .ease
                .as_deref()
                .and_then(crate::tween::Ease::parse)
                .unwrap_or_default(),
            repeat: self.repeat,
        }
    }
}

fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}

fn is_default_target(t: &BindTargetDef) -> bool {
    *t == BindTargetDef::default()
}

fn is_default_mode(m: &BindMode) -> bool {
    *m == BindMode::default()
}

impl BindDef {
    /// The runtime binding this definition spawns as.
    pub fn to_bind(&self) -> Bind {
        let mut bind = if let Some(token) = &self.theme {
            Bind::theme(token, self.format.as_deref())
        } else if let Some(animation) = &self.animation {
            Bind::animation(animation.to_animation(), self.format.as_deref())
        } else if self.source.starts_with('.') {
            Bind::item(&self.source, self.format.as_deref())
        } else {
            Bind::text(&self.source, self.format.as_deref())
        };
        bind.target = self.target.into();
        bind.mode = self.mode;
        bind.invert = self.invert;
        bind.converter.clone_from(&self.converter);
        bind
    }
}

/// Serialization-friendly bind target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BindTargetDef {
    /// Entity's text.
    #[default]
    Text,
    /// Entity's progress bar.
    Progress,
    /// Entity's opacity.
    Opacity,
    /// Entity's toggle.
    Checked,
    /// Entity's slider value (v3).
    Value,
    /// Entity's text input content (v3).
    Input,
    /// Entity's dropdown selection or tab bar active index (v3).
    Selected,
    /// The node is shown when true, `display: none` when false (v3).
    Visible,
    /// The node is enabled when true, `Disabled` when false (v3).
    Enabled,
}

impl BindTargetDef {
    /// Every target, in declaration order (designer pickers, lint).
    pub const ALL: [BindTargetDef; 9] = [
        Self::Text,
        Self::Progress,
        Self::Opacity,
        Self::Checked,
        Self::Value,
        Self::Input,
        Self::Selected,
        Self::Visible,
        Self::Enabled,
    ];

    /// The `.ui.ron` spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Progress => "progress",
            Self::Opacity => "opacity",
            Self::Checked => "checked",
            Self::Value => "value",
            Self::Input => "input",
            Self::Selected => "selected",
            Self::Visible => "visible",
            Self::Enabled => "enabled",
        }
    }
}

impl From<BindTargetDef> for BindTarget {
    fn from(t: BindTargetDef) -> Self {
        match t {
            BindTargetDef::Text => Self::Text,
            BindTargetDef::Progress => Self::Progress,
            BindTargetDef::Opacity => Self::Opacity,
            BindTargetDef::Checked => Self::Checked,
            BindTargetDef::Value => Self::Value,
            BindTargetDef::Input => Self::Input,
            BindTargetDef::Selected => Self::Selected,
            BindTargetDef::Visible => Self::Visible,
            BindTargetDef::Enabled => Self::Enabled,
        }
    }
}

/// Registry of `.ui.ron` migrators: `from_version -> upgrade` functions registered by the game
/// (for example from a pre-v1 in-house shape). The engine's own steps (v1 → v2, v2 → v3; ADR-1381, ADR-1811) are
/// built in and run first, so a game step registered for a version the engine already migrates
/// is never used.
#[derive(Resource, Default, Clone)]
pub struct UiMigrators {
    upgrades: Vec<(u32, UiMigrator)>,
    /// Registered source names for `cargo regia doctor`-style diagnostics.
    pub(crate) names: Vec<String>,
}

/// A migrator function: rewrites the RON *text* in place, returning the new version.
///
/// Text (not `ron::Value`) is required because untyped RON values collapse unit enum
/// variants such as `Auto` / `button` to `Unit`, which cannot be round-tripped.
pub type UiMigrator = Arc<dyn Fn(&mut String) -> Result<u32, String> + Send + Sync>;

impl UiMigrators {
    /// Registers a migrator that upgrades files declaring `from_version`.
    pub fn register(&mut self, from_version: u32, name: &str, migrate: UiMigrator) {
        self.upgrades.push((from_version, migrate));
        self.names
            .push(format!("v{from_version}→{} {name}", from_version + 1));
    }

    /// Runs migration steps from `found` up to [`UI_RON_VERSION`]: the built-in step for a version
    /// when there is one, else the registered one. Errors when a step is missing or does not
    /// raise the version (a step that returned its own input version would loop forever).
    pub fn migrate(&self, source: &mut String, found: u32) -> Result<u32, String> {
        let mut version = found;
        while version < UI_RON_VERSION {
            let next = if let Some((_, step)) = BUILTIN_UI_STEPS.iter().find(|(from, _)| *from == version) {
                step(source)?
            } else if let Some((_, step)) = self.upgrades.iter().find(|(from, _)| *from == version) {
                step(source)?
            } else {
                return Err(format!("no migrator from v{version} (current v{UI_RON_VERSION})"));
            };
            if next <= version {
                return Err(format!("migrator from v{version} returned v{next}"));
            }
            version = next;
        }
        Ok(version)
    }
}

/// RON options for `.ui.ron`: bare values fill `Option<T>` (`name: "hud"` → `Some("hud")`).
fn ui_ron_options() -> ron::Options {
    ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
}

/// Parses a `.ui.ron` document. Older versions are upgraded first (the built-in steps, then the
/// ones in `migrators`); newer versions, versions no step upgrades and unknown fields are rejected.
pub fn parse_ui_ron(source: &str, path: &str, migrators: &UiMigrators) -> Result<UiFile, crate::UiError> {
    let text = migrated_ui_text(source, path, migrators)?;
    let file: UiFile = ui_ron_options()
        .from_str(&text)
        .map_err(|source| crate::UiError::Ron {
            path: path.to_owned(),
            source,
        })?;
    if file.version != UI_RON_VERSION {
        return Err(crate::UiError::UnsupportedVersion {
            path: path.to_owned(),
            found: file.version,
            supported: UI_RON_VERSION,
        });
    }
    check_pictures(&file.root, path)?;
    check_pivot(&file.root, path)?;
    Ok(file)
}

/// `pivot`, when set, is a pair of finite numbers. NaN and infinity are refused. A ratio outside
/// 0..=1 is allowed: the pivot may sit outside the rect.
fn check_pivot(node: &UiNodeDef, path: &str) -> Result<(), crate::UiError> {
    if let Some(pivot) = node.pivot
        && (!pivot[0].is_finite() || !pivot[1].is_finite())
    {
        return Err(crate::UiError::Invalid {
            path: path.to_owned(),
            reason: format!(
                "node {:?}: pivot must be finite",
                node.name.as_deref().unwrap_or("")
            ),
        });
    }
    for child in &node.children {
        check_pivot(child, path)?;
    }
    for nodes in node.slots.values() {
        for child in nodes {
            check_pivot(child, path)?;
        }
    }
    Ok(())
}

/// Every mask of a node tree names exactly one image, and every image at most one of a renderer
/// id, an asset path and an icon name, with no empty name and finite, non-negative insets.
fn check_pictures(node: &UiNodeDef, path: &str) -> Result<(), crate::UiError> {
    let fail = |what: &str| {
        Err(crate::UiError::Invalid {
            path: path.to_owned(),
            reason: format!("node {:?}: {what}", node.name.as_deref().unwrap_or("")),
        })
    };
    let empty = |name: &Option<String>| name.as_deref().is_some_and(|n| n.trim().is_empty());
    if let Some(mask) = &node.mask {
        let named = usize::from(mask.image.is_some())
            + usize::from(mask.asset.is_some())
            + usize::from(mask.icon.is_some());
        if named != 1 {
            return fail("a mask needs exactly one of `image`, `asset` and `icon`");
        }
        if empty(&mask.asset) || empty(&mask.icon) {
            return fail("a mask `asset` or `icon` is empty");
        }
    }
    if let Some(image) = &node.image {
        let named = usize::from(image.id != 0)
            + usize::from(image.asset.is_some())
            + usize::from(image.icon.is_some());
        if named > 1 {
            return fail("an image names at most one of `id`, `asset` and `icon`");
        }
        if empty(&image.asset) || empty(&image.icon) {
            return fail("an image `asset` or `icon` is empty");
        }
        if let Some((l, t, r, b)) = image.nine_slice
            && [l, t, r, b].iter().any(|v| !v.is_finite() || *v < 0.0)
        {
            return fail("image `nine_slice` insets must be finite and not negative");
        }
    }
    node.children
        .iter()
        .chain(node.slots.values().flatten())
        .try_for_each(|c| check_pictures(c, path))
}

/// Upgrades `.ui.ron` source text to [`UI_RON_VERSION`] with the built-in steps (what
/// `regia upgrade-project` writes back): the v1 → v2 step rewrites only the root `version`
/// number, so comments and layout survive byte for byte. The result is parsed before it is
/// returned, so text the loader would reject is never handed back; a current file comes back
/// unchanged, a newer one is an error.
pub fn upgrade_ui_ron(source: &str, path: &str) -> Result<String, crate::UiError> {
    let migrators = UiMigrators::default();
    let text = migrated_ui_text(source, path, &migrators)?;
    parse_ui_ron(&text, path, &migrators)?;
    Ok(text)
}

/// `source` upgraded to [`UI_RON_VERSION`] (unchanged when it is current). The version is read
/// from the top-level struct before the body is checked, so an old body still reports its version.
fn migrated_ui_text(source: &str, path: &str, migrators: &UiMigrators) -> Result<String, crate::UiError> {
    #[derive(Deserialize)]
    struct VersionOnly {
        version: u32,
    }
    let opts = ui_ron_options();
    let peek: ron::Value = opts.from_str(source).map_err(|source| crate::UiError::Ron {
        path: path.to_owned(),
        source,
    })?;
    let found = VersionOnly::deserialize(peek)
        .ok()
        .map(|v| v.version)
        .unwrap_or(0);
    if found > UI_RON_VERSION {
        return Err(crate::UiError::UnsupportedVersion {
            path: path.to_owned(),
            found,
            supported: UI_RON_VERSION,
        });
    }
    let mut text = source.to_owned();
    if found < UI_RON_VERSION {
        migrators
            .migrate(&mut text, found)
            .map_err(|reason| crate::UiError::UnsupportedVersion {
                path: format!("{path} ({reason})"),
                found,
                supported: UI_RON_VERSION,
            })?;
    }
    Ok(text)
}

/// Named UI templates loaded from `assets://ui/*.ui.ron`.
///
/// `templates` hold the expanded trees (no component instances left; what [`spawn_ui`] and the
/// repeat pass build). The source documents, the components each template uses and its expansion
/// diagnostics are kept beside them (spec 111 § 4), so a changed component re-expands its users.
#[derive(Resource, Default)]
pub struct UiLibrary {
    /// Templates by name (file stem), expanded.
    pub templates: HashMap<String, UiFile>,
    /// Source documents by template name (instances kept as references).
    sources: HashMap<String, (String, UiFile)>,
    /// Component paths each template reaches, directly or through other components.
    uses: HashMap<String, BTreeSet<String>>,
    /// Expansion diagnostics by template name.
    diagnostics: HashMap<String, Vec<crate::ui_component::ComponentDiagnostic>>,
}

impl UiLibrary {
    /// Loads every `.ui.ron` under `assets://ui/` (best effort; errors are logged) and expands
    /// their component instances, reading components through `vfs` (`assets://<use path>`).
    pub fn load_from_vfs(&mut self, vfs: &regia_platform::Vfs, migrators: &UiMigrators) {
        let Ok(dir) = regia_platform::VfsPath::parse("assets://ui") else {
            return;
        };
        let Ok(entries) = vfs.list_dir(&dir) else {
            return;
        };
        let mut parsed = Vec::new();
        for entry in entries
            .iter()
            .filter(|e| !e.is_dir && e.name.ends_with(".ui.ron"))
        {
            let Ok(path) = regia_platform::VfsPath::parse(&format!("assets://ui/{}", entry.name)) else {
                continue;
            };
            let Ok(bytes) = vfs.read_sync(&path) else {
                tracing::warn!(target: "regia_ui", "cannot read {path}");
                continue;
            };
            let source = String::from_utf8_lossy(&bytes);
            let name = entry.name.trim_end_matches(".ui.ron").to_owned();
            match parse_ui_ron(&source, &path.to_string(), migrators) {
                Ok(file) => {
                    tracing::debug!(target: "regia_ui", "ui template loaded: {name}");
                    parsed.push((name, format!("ui/{}", entry.name), file));
                }
                Err(err) => tracing::error!(target: "regia_ui", "{err}"),
            }
        }
        let mut load = crate::ui_component::text_loader(vfs_reader(vfs), migrators);
        for (name, document, file) in parsed {
            self.insert_with(&name, &document, file, &mut load);
        }
    }

    /// Stores `file` (from `document`, its path under `assets/`) as template `name`, expanding its
    /// instances with `load`. Returns the expansion diagnostics.
    pub fn insert_with(
        &mut self,
        name: &str,
        document: &str,
        file: UiFile,
        load: &mut crate::ui_component::ComponentLoader<'_>,
    ) -> &[crate::ui_component::ComponentDiagnostic] {
        let expansion = crate::ui_component::expand(&file, document, load);
        self.templates.insert(
            name.to_owned(),
            UiFile {
                version: file.version,
                component: file.component.clone(),
                root: expansion.root,
                variants: file.variants.clone(),
                back: file.back,
            },
        );
        self.sources.insert(name.to_owned(), (document.to_owned(), file));
        self.uses.insert(name.to_owned(), expansion.uses);
        let diagnostics = self.diagnostics.entry(name.to_owned()).or_default();
        *diagnostics = expansion.diagnostics;
        diagnostics
    }

    /// Re-expands template `name` from its source document with `load` (a component it uses
    /// changed). `false` when the template has no source document.
    pub fn reexpand(&mut self, name: &str, load: &mut crate::ui_component::ComponentLoader<'_>) -> bool {
        let Some((document, file)) = self.sources.get(name).cloned() else {
            return false;
        };
        self.insert_with(name, &document, file, load);
        true
    }

    /// Whether `name` is loaded.
    pub fn contains(&self, name: &str) -> bool {
        self.templates.contains_key(name)
    }

    /// The source document of template `name` (instances kept as references) and its path.
    pub fn source(&self, name: &str) -> Option<(&str, &UiFile)> {
        self.sources.get(name).map(|(d, f)| (d.as_str(), f))
    }

    /// The component paths template `name` reaches.
    pub fn uses_of(&self, name: &str) -> Option<&BTreeSet<String>> {
        self.uses.get(name)
    }

    /// The expansion diagnostics of template `name` (empty when it has none or is unknown).
    pub fn diagnostics_of(&self, name: &str) -> &[crate::ui_component::ComponentDiagnostic] {
        self.diagnostics.get(name).map_or(&[], Vec::as_slice)
    }

    /// The templates that use `component` (a path under `assets/`), sorted.
    pub fn users_of(&self, component: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .uses
            .iter()
            .filter(|(_, uses)| uses.contains(component))
            .map(|(name, _)| name.clone())
            .collect();
        out.sort();
        out
    }

    /// The source documents that declare `component:` as `(template name, document path, file)`,
    /// sorted by path.
    pub fn components(&self) -> Vec<(&str, &str, &UiFile)> {
        let mut out: Vec<(&str, &str, &UiFile)> = self
            .sources
            .iter()
            .filter(|(_, (_, f))| f.component.is_some())
            .map(|(n, (d, f))| (n.as_str(), d.as_str(), f))
            .collect();
        out.sort_by(|a, b| a.1.cmp(b.1));
        out
    }

    /// Every template name, sorted.
    pub fn names(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.templates.keys().map(String::as_str).collect();
        out.sort_unstable();
        out
    }
}

/// A component text reader over the VFS (`path` under `assets/` → `assets://<path>`).
fn vfs_reader(vfs: &regia_platform::Vfs) -> impl FnMut(&str) -> Result<String, String> + '_ {
    move |path: &str| {
        let at = regia_platform::VfsPath::parse(&format!("assets://{path}"))
            .map_err(|e| format!("{path}: {e}"))?;
        let bytes = vfs.read_sync(&at).map_err(|e| format!("{path}: {e}"))?;
        String::from_utf8(bytes.to_vec()).map_err(|e| format!("{path}: {e}"))
    }
}

/// The component text reader of a world: the project's `assets/` directory when an
/// [`regia_asset::AssetServer`] is present (dev builds, hot reload), else the VFS.
pub fn world_component_reader(world: &World) -> Box<dyn FnMut(&str) -> Result<String, String>> {
    if let Some(server) = world.get_resource::<regia_asset::AssetServer>() {
        let assets = server.project_root().join("assets");
        // A component path comes from content: the same rule as the VFS branch (no `..`, absolute,
        // drive or UNC form), and no symlink leading out of `assets/` (ADR-7388, audit
        // rc1-sec-paths-z finding 7).
        return Box::new(move |path: &str| {
            let file =
                regia_core::project_path::resolve_read(&assets, path).map_err(|e| format!("{path}: {e}"))?;
            std::fs::read_to_string(file).map_err(|e| format!("{path}: {e}"))
        });
    }
    if let Some(vfs) = world.get_resource::<regia_platform::Vfs>().cloned() {
        return Box::new(move |path: &str| vfs_reader(&vfs)(path));
    }
    Box::new(|path: &str| Err(format!("{path}: no asset server or VFS to read components from")))
}

/// Stores `file` as template `name` (from `document`, its path under `assets/`), expanding its
/// component instances through [`world_component_reader`]. Returns the diagnostics.
pub fn insert_ui_template(
    world: &mut World,
    name: &str,
    document: &str,
    file: UiFile,
) -> Vec<crate::ui_component::ComponentDiagnostic> {
    let migrators = world.get_resource::<UiMigrators>().cloned().unwrap_or_default();
    let reader = world_component_reader(world);
    let mut load = crate::ui_component::text_loader(reader, &migrators);
    world.init_resource::<UiLibrary>();
    world
        .resource_mut::<UiLibrary>()
        .insert_with(name, document, file, &mut load)
        .to_vec()
}

/// `Startup`: fill the library from the VFS.
pub fn load_ui_assets(world: &mut World) {
    let migrators = world.get_resource::<UiMigrators>().cloned().unwrap_or_default();
    let Some(vfs) = world.get_resource::<regia_platform::Vfs>().cloned() else {
        return;
    };
    world.resource_mut::<UiLibrary>().load_from_vfs(&vfs, &migrators);
}

/// Spawns a template (by library name) as a new UI root. Errors are logged, not panicked on.
pub fn spawn_ui(world: &mut World, name: &str) -> Result<Entity, crate::UiError> {
    let Some(file) = world
        .get_resource::<UiLibrary>()
        .and_then(|l| l.templates.get(name))
    else {
        return Err(crate::UiError::AssetNotFound(format!(
            "ui template {name:?} not loaded"
        )));
    };
    // The variant the window picks applies on top of the base (P-271, ADR-7858).
    let size = world
        .get_resource::<crate::UiScales>()
        .map_or([0.0; 2], crate::layout_variants::logical_window_size);
    let (root_def, variant) = crate::layout_variants::resolve(file, size);
    for problem in &variant.problems {
        tracing::warn!(target: "regia_ui", "ui template {name:?} layout variant: {problem}");
    }
    let back = file.back;
    let entity = world
        .spawn((
            UiRoot::default(),
            NodeStyle::new(root_def.style.clone()),
            Themed::default(),
            UiTemplate(name.to_owned()),
            variant,
        ))
        .id();
    if let Some(back) = back {
        world.entity_mut(entity).insert(back.to_component());
    }
    spawn_children(world, entity, &root_def.children);
    crate::inspect::tag_document_nodes(world, entity, name);
    // Declared focus neighbors are usable the frame the document spawns (ADR-7827).
    resolve_declared_nav(world);
    Ok(entity)
}

/// The library template a root was spawned from ([`spawn_ui`]); a live reload of that
/// `.ui.ron` respawns the root.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct UiTemplate(pub String);

/// `.ui.ron` files changed on disk this frame, waiting for [`apply_ui_reloads`].
#[derive(Resource, Default)]
pub struct PendingUiReloads(Vec<std::path::PathBuf>);

impl PendingUiReloads {
    /// Queues a changed `.ui.ron` (an absolute path under the project's `assets/`).
    pub fn push(&mut self, path: std::path::PathBuf) {
        self.0.push(path);
    }
}

/// `Update`: collects changed `.ui.ron` files (dev builds, `docs/specs/12-assets.md` § 8).
pub fn collect_ui_changes(
    mut changes: MessageReader<regia_asset::SourceAssetChanged>,
    mut pending: ResMut<PendingUiReloads>,
) {
    for change in changes.read() {
        if change.kind == regia_asset::SourceKind::UiLayout {
            pending.0.push(change.path.clone());
        }
    }
}

/// The path under `assets/` of a changed file (`…/assets/ui/hud.ui.ron` → `ui/hud.ui.ron`).
fn asset_relative(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    match text.rfind("/assets/") {
        Some(at) => text[at + "/assets/".len()..].to_owned(),
        None => text.rsplit('/').next().unwrap_or_default().to_owned(),
    }
}

/// Focus and scroll state of a UI tree by node key (spec 111 § 4.1), kept across a respawn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeptUiState {
    /// Key of the focused node, when it was in the tree.
    pub focus: Option<String>,
    /// Scroll offsets by key.
    pub scroll: Vec<(String, [f32; 2])>,
}

/// The key of every node under `root` (names from the root, `#<index>` for unnamed nodes; the root
/// itself is `""`), in tree order.
pub fn ui_node_keys(world: &World, root: Entity) -> Vec<(String, Entity)> {
    let mut out = vec![(String::new(), root)];
    let mut i = 0;
    while i < out.len() {
        let (key, entity) = out[i].clone();
        if let Some(children) = world.get::<Children>(entity) {
            for (index, child) in children.iter().enumerate() {
                let own = world
                    .get::<NodeName>(child)
                    .map_or_else(|| format!("#{index}"), |n| n.0.clone());
                let k = if key.is_empty() {
                    own
                } else {
                    format!("{key}/{own}")
                };
                out.push((k, child));
            }
        }
        i += 1;
    }
    out
}

/// Records the focus and scroll state of the tree under `root`.
pub fn capture_ui_state(world: &World, root: Entity) -> KeptUiState {
    let focused = world
        .get_resource::<crate::focus::FocusState>()
        .and_then(|f| f.focused);
    let mut kept = KeptUiState::default();
    for (key, entity) in ui_node_keys(world, root) {
        if Some(entity) == focused {
            kept.focus = Some(key.clone());
        }
        if let Some(offset) = world.get::<crate::scroll::ScrollOffset>(entity)
            && offset.0 != [0.0, 0.0]
        {
            kept.scroll.push((key, offset.0));
        }
    }
    kept
}

/// Gives the nodes under `root` whose keys were recorded their focus and scroll state back; keys
/// that no longer exist are dropped. Returns how many were restored.
pub fn restore_ui_state(world: &mut World, root: Entity, kept: &KeptUiState) -> usize {
    let keys: HashMap<String, Entity> = ui_node_keys(world, root).into_iter().collect();
    let mut restored = 0;
    if let Some(key) = &kept.focus
        && let Some(&entity) = keys.get(key)
        && let Some(mut focus) = world.get_resource_mut::<crate::focus::FocusState>()
    {
        focus.focused = Some(entity);
        restored += 1;
    }
    for (key, offset) in &kept.scroll {
        if let Some(&entity) = keys.get(key)
            && let Some(mut scroll) = world.get_mut::<crate::scroll::ScrollOffset>(entity)
        {
            scroll.0 = *offset;
            restored += 1;
        }
    }
    restored
}

/// Respawns every root spawned from template `name`, keeping each root's settings and its focus
/// and scroll state by key. Returns how many roots were respawned.
pub fn respawn_ui_roots(world: &mut World, name: &str) -> usize {
    let mut roots = world.query::<(Entity, &UiTemplate, &UiRoot)>();
    let live: Vec<(Entity, UiRoot)> = roots
        .iter(world)
        .filter(|(_, template, _)| template.0 == name)
        .map(|(entity, _, root)| (entity, root.clone()))
        .collect();
    let mut respawned = 0;
    for (entity, root) in live {
        if respawn_root(world, entity, name, root) {
            respawned += 1;
        }
    }
    respawned
}

/// Respawns the one template root `entity` (its [`UiTemplate`] and [`UiRoot`] settings, focus and
/// scroll kept by key), leaving the template's other roots alone. `false` when `entity` is not a
/// template root or the template no longer spawns.
pub(crate) fn respawn_ui_root(world: &mut World, entity: Entity) -> bool {
    let Some(name) = world.get::<UiTemplate>(entity).map(|t| t.0.clone()) else {
        return false;
    };
    let Some(root) = world.get::<UiRoot>(entity).cloned() else {
        return false;
    };
    respawn_root(world, entity, &name, root)
}

fn respawn_root(world: &mut World, entity: Entity, name: &str, root: UiRoot) -> bool {
    let kept = capture_ui_state(world, entity);
    let focus_was_inside = kept.focus.is_some();
    world.despawn(entity);
    if focus_was_inside && let Some(mut focus) = world.get_resource_mut::<crate::focus::FocusState>() {
        focus.focused = None;
    }
    let Ok(fresh) = spawn_ui(world, name) else {
        return false;
    };
    world.entity_mut(fresh).insert(root);
    restore_ui_state(world, fresh, &kept);
    true
}

/// `Update`, after [`collect_ui_changes`]: re-parses each changed layout into the [`UiLibrary`],
/// re-expands every template that uses it as a component (spec 111 § 4.1) and respawns the roots
/// spawned from those templates (same [`UiRoot`] settings, focus and scroll kept by key). A file
/// that does not parse keeps the old template and the live roots.
pub fn apply_ui_reloads(world: &mut World) {
    let paths = std::mem::take(&mut world.resource_mut::<PendingUiReloads>().0);
    if paths.is_empty() {
        return;
    }
    let migrators = world.get_resource::<UiMigrators>().cloned().unwrap_or_default();
    for path in paths {
        let started = std::time::Instant::now();
        let shown = match world.get_resource::<regia_asset::AssetServer>() {
            Some(server) => regia_asset::display_path(server.project_root(), &path),
            None => path.display().to_string(),
        };
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.trim_end_matches(".ui.ron").to_owned())
            .unwrap_or_default();
        let rel = asset_relative(&path);
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| parse_ui_ron(&text, &shown, &migrators).map_err(|e| e.to_string()));
        let outcome = match parsed {
            Ok(file) => {
                let mut load = crate::ui_component::text_loader(world_component_reader(world), &migrators);
                let mut affected = world.resource::<UiLibrary>().users_of(&rel);
                {
                    let mut library = world.resource_mut::<UiLibrary>();
                    // The file is a template of its own too (as before components).
                    library.insert_with(&name, &rel, file, &mut load);
                    affected.push(name.clone());
                    for user in &affected {
                        if user != &name {
                            library.reexpand(user, &mut load);
                        }
                    }
                }
                affected.sort();
                affected.dedup();
                let mut respawned = 0;
                for template in &affected {
                    respawned += respawn_ui_roots(world, template);
                }
                tracing::info!(target: "regia_ui", "reloaded ui {name} from {shown} ({} template(s), {respawned} root(s) respawned)", affected.len());
                regia_asset::WatchOutcome::Reloaded
            }
            Err(err) => {
                tracing::error!(target: "regia_ui", "{shown}: {err} (keeping the previous layout)");
                regia_asset::WatchOutcome::Error(err)
            }
        };
        if let Some(mut status) = world.get_resource_mut::<regia_asset::AssetWatchStatus>() {
            status.record(regia_asset::WatchEntry {
                path: shown,
                kind: regia_asset::SourceKind::UiLayout,
                millis: started.elapsed().as_millis() as u64,
                outcome,
            });
        }
    }
}

/// Spawns `def` (and its subtree) as a child of `parent`.
pub fn spawn_node(world: &mut World, parent: Entity, def: &UiNodeDef) -> Entity {
    if def.use_component.is_some() {
        // An instance that reached the spawner unexpanded (code-built trees): expand it here.
        let migrators = world.get_resource::<UiMigrators>().cloned().unwrap_or_default();
        let mut load = crate::ui_component::text_loader(world_component_reader(world), &migrators);
        let expansion = crate::ui_component::expand_node(def, "<spawn>", &mut load);
        return spawn_node(world, parent, &expansion.root);
    }
    let mut extra_opacity: Option<Opacity> = None;
    let mut extra_border: Option<Border> = None;
    let mut extra_background: Option<Background> = None;
    let mut extra_shadow: Option<Shadow> = None;
    let mut extra_image: Option<UiImage> = None;
    let mut extra_text: Option<Text> = None;
    let mut extra_bind: Option<Bind> = None;
    let mut extra_scroll: Option<(ScrollView, crate::scroll::ScrollOffset)> = None;
    let mut extra_list: Option<(ListView, ItemCount)> = None;
    let mut extra_classes = def.classes.clone();
    let mut extra_interactable = def.interactable.then_some(Interactable::default());
    let mut extra_misc: Vec<Box<dyn BundleLike>> = Vec::new();

    if let Some(bg) = &def.background {
        extra_background = Some(Background {
            kind: match bg {
                BackgroundDef::Color(c) => BackgroundKind::Color(*c),
                BackgroundDef::Gradient { from, to, angle } => BackgroundKind::Gradient {
                    start: *from,
                    end: *to,
                    angle: *angle,
                },
            },
        });
    }
    if let Some(b) = &def.border {
        extra_border = Some(Border {
            color: b.color,
            widths: [b.width; 4],
            radius: [b.radius; 4],
        });
    }
    if let Some(s) = &def.shadow {
        extra_shadow = Some(Shadow {
            color: s.color,
            blur: s.blur,
            offset: [s.offset.0, s.offset.1],
        });
    }
    if let Some(img) = &def.image {
        // v11 `fit` (ADR-9980): stretch drops the aspect, cover crops instead of letterboxing.
        let fit = img.effective_fit();
        let keep_aspect = fit != crate::node::ImageFit::Stretch;
        // `tile` (ADR-7751) repeats the picture at its native size.
        if matches!(fit, crate::node::ImageFit::Cover | crate::node::ImageFit::Tile) {
            extra_misc.push(Box::new((crate::node::UiImageFit(fit),)));
        }
        match img.source() {
            // A named picture: the placeholder draws until the background load lands (ADR-3148).
            Some(source) => {
                let asset = crate::ui_image_assets::UiImageAsset {
                    source,
                    tint: img.tint,
                    preserve_aspect: keep_aspect,
                    nine_slice: img.nine_slice.map(|(l, t, r, b)| [l, t, r, b]),
                };
                extra_image = Some(asset.placeholder_image());
                extra_misc.push(Box::new(asset));
            }
            None => match img.nine_slice {
                Some((l, t, r, b)) => {
                    extra_background = Some(Background {
                        kind: BackgroundKind::Image {
                            id: img.id,
                            uv: [0.0, 0.0, 1.0, 1.0],
                            nine_slice: Some([l, t, r, b]),
                        },
                    });
                }
                None => {
                    extra_image = Some(UiImage {
                        id: img.id,
                        uv: [0.0, 0.0, 1.0, 1.0],
                        tint: img.tint,
                        preserve_aspect: keep_aspect,
                    });
                }
            },
        }
    }
    if let Some(t) = &def.text {
        extra_text = Some(Text {
            spans: text_def_spans(t),
            align: t.align,
            wrap: t.wrap,
            overflow: t.overflow,
            line_height: t.line_height,
        });
    }
    if let Some(opacity) = def.opacity {
        extra_opacity = Some(Opacity(opacity));
    }
    if let Some(b) = &def.bind {
        extra_bind = Some(b.to_bind());
    }
    if let Some(widget) = &def.widget {
        match widget {
            WidgetDef::Button => {
                // The widget's own type class goes first: a later class wins in the resolver, so
                // an authored variation (`classes: ["primary"]`) styles the button (ADR-8303).
                if !extra_classes.iter().any(|c| c == "button") {
                    extra_classes.insert(0, "button".to_owned());
                }
                extra_misc.push(Box::new((Button, crate::focus::Focusable::default())));
                extra_interactable = Some(Interactable::default());
            }
            WidgetDef::Toggle(checked) => {
                extra_misc.push(Box::new((
                    Toggle { checked: *checked },
                    crate::focus::Focusable::default(),
                )));
                extra_interactable = Some(Interactable::default());
            }
            WidgetDef::Slider(v, min, max, step) => {
                extra_misc.push(Box::new((
                    Slider {
                        value: *v,
                        min: *min,
                        max: *max,
                        step: *step,
                    },
                    crate::focus::Focusable::default(),
                )));
                extra_interactable = Some(Interactable::default());
                if !extra_classes.iter().any(|c| c == "slider-track") {
                    extra_classes.insert(0, "slider-track".to_owned());
                }
            }
            WidgetDef::Progress(v) => {
                extra_misc.push(Box::new((ProgressBar {
                    value: v.clamp(0.0, 1.0),
                },)));
            }
            WidgetDef::TextInput(placeholder, multiline, options) => {
                extra_misc.push(Box::new((
                    TextInput {
                        placeholder: placeholder.clone(),
                        multiline: *multiline,
                        max_chars: options.max_chars,
                        secret: options.secret,
                        read_only: options.read_only,
                        filter: options.filter.to_filter(),
                        ..Default::default()
                    },
                    crate::focus::Focusable::default(),
                )));
                extra_interactable = Some(Interactable::default());
            }
            WidgetDef::Dropdown(options, selected) => {
                extra_misc.push(Box::new((
                    Dropdown {
                        options: options.clone(),
                        selected: *selected,
                        open: false,
                    },
                    crate::focus::Focusable::default(),
                )));
                extra_interactable = Some(Interactable::default());
            }
            WidgetDef::TabBar(tabs, active) => {
                extra_misc.push(Box::new(TabBar {
                    tabs: tabs.clone(),
                    active: *active,
                }));
            }
            WidgetDef::Modal(open, close) => {
                extra_misc.push(Box::new((
                    Modal {
                        open: *open,
                        close_on_cancel: *close,
                    },
                    crate::focus::FocusScope,
                )));
            }
            WidgetDef::Scroll(inertia, scrollbar) => {
                extra_scroll = Some((
                    ScrollView {
                        inertia: *inertia,
                        show_scrollbar: *scrollbar,
                        ..Default::default()
                    },
                    crate::scroll::ScrollOffset::default(),
                ));
            }
            WidgetDef::List(count, item_height) => {
                extra_list = Some((
                    ListView {
                        item_height: *item_height,
                        overscan: 2,
                        first_visible: 0,
                        pool: Vec::new(),
                        dirty: true,
                    },
                    ItemCount(*count),
                ));
            }
            WidgetDef::Tooltip(text, delay) => {
                extra_misc.push(Box::new(Tooltip {
                    text: text.clone(),
                    delay: *delay,
                    ..Default::default()
                }));
            }
            WidgetDef::Focusable => {
                extra_misc.push(Box::new((crate::focus::Focusable::default(),)));
            }
            WidgetDef::Stick(action, radius) => {
                extra_misc.push(Box::new((VirtualStick {
                    action: action.clone(),
                    radius: *radius,
                    ..Default::default()
                },)));
                extra_interactable = Some(Interactable::default());
            }
            WidgetDef::VButton(action) => {
                extra_misc.push(Box::new((VirtualButton {
                    action: action.clone(),
                },)));
                extra_interactable = Some(Interactable::default());
            }
            WidgetDef::PopupMenu(entries) => {
                extra_misc.push(Box::new((
                    crate::menu::PopupMenu {
                        entries: entries.iter().map(MenuEntryDef::to_entry).collect(),
                        ..Default::default()
                    },
                    crate::focus::Focusable::default(),
                )));
                extra_interactable = Some(Interactable::default());
            }
            WidgetDef::ContextMenu(entries) => {
                extra_misc.push(Box::new((crate::menu::ContextMenu {
                    entries: entries.iter().map(MenuEntryDef::to_entry).collect(),
                    ..Default::default()
                },)));
                extra_interactable = Some(Interactable::default());
            }
            WidgetDef::MenuBar(menus) => {
                if !extra_classes.iter().any(|c| c == "MenuBar") {
                    extra_classes.insert(0, "MenuBar".to_owned());
                }
                extra_misc.push(Box::new((crate::menu::MenuBar {
                    menus: menus.iter().map(MenuEntryDef::to_item).collect(),
                },)));
            }
            WidgetDef::LobbyList(d) => {
                let mut labels = crate::game_widgets::LobbyListLabels::default();
                for (word, def) in [
                    (&mut labels.title, &d.title),
                    (&mut labels.refresh, &d.refresh),
                    (&mut labels.create, &d.create),
                    (&mut labels.empty, &d.empty),
                    (&mut labels.ready, &d.ready),
                    (&mut labels.leave, &d.leave),
                    (&mut labels.launch, &d.launch),
                ] {
                    if !def.is_empty() {
                        word.clone_from(def);
                    }
                }
                extra_misc.push(Box::new((crate::game_widgets::LobbyList {
                    labels,
                    ..Default::default()
                },)));
            }
            WidgetDef::Scoreboard(d) => {
                extra_misc.push(Box::new((crate::game_widgets::Scoreboard {
                    title: d.title.clone(),
                    table: crate::game_widgets::ScoreTable {
                        columns: d
                            .columns
                            .iter()
                            .map(|c| crate::game_widgets::ScoreColumn {
                                key: c.key.clone(),
                                title: c.title.clone(),
                                width: c.width,
                            })
                            .collect(),
                        ..Default::default()
                    },
                    sort_column: d.sort_column,
                    descending: d.descending,
                    group_by_team: d.group_by_team,
                    visible: d.hold_action.is_empty(),
                    hold_action: d.hold_action.clone(),
                    from_rules: d.from_rules,
                },)));
            }
            WidgetDef::Radar(d) => {
                extra_misc.push(Box::new((d.to_radar(),)));
            }
            WidgetDef::MarkerLayer(d) => {
                extra_misc.push(Box::new((
                    crate::game_widgets::MarkerLayer {
                        layer_mask: d.layer_mask,
                        edge_margin: d.edge_margin,
                        max_markers: d.max_markers as usize,
                        ..Default::default()
                    },
                    crate::node::PickThrough,
                )));
            }
            // The v11 containers (ADR-7827): their extra children are made by `spawn_container`.
            WidgetDef::Split(d) => {
                extra_misc.push(Box::new((d.to_split(),)));
            }
            WidgetDef::Foldable(d) => {
                extra_misc.push(Box::new((crate::default_widgets::Foldable {
                    open: d.open,
                    group: d.group.clone(),
                },)));
            }
            WidgetDef::Tabs(_) => {
                extra_misc.push(Box::new((crate::default_widgets::TabPages,)));
            }
        }
    }

    let mut style = authored_style(def);
    match &def.widget {
        Some(WidgetDef::Split(d)) => {
            style.direction = match d.axis {
                SplitAxisDef::Horizontal => crate::style::FlexDirection::Row,
                SplitAxisDef::Vertical => crate::style::FlexDirection::Column,
            };
        }
        Some(WidgetDef::Foldable(_) | WidgetDef::Tabs(_)) => {
            style.direction = crate::style::FlexDirection::Column;
        }
        _ => {}
    }
    let mut entity = world.spawn((UiNode, NodeStyle::new(style), Themed::default()));
    entity.insert(Classes(extra_classes));
    if let Some(c) = &def.name {
        entity.insert(NodeName(c.clone()));
    }
    if let Some(bg) = extra_background {
        entity.insert(bg);
    }
    if let Some(b) = extra_border {
        entity.insert(b);
    }
    if let Some(s) = extra_shadow {
        entity.insert(s);
    }
    if let Some(i) = extra_image {
        entity.insert(i);
    }
    if let Some(t) = extra_text {
        entity.insert(t);
    }
    if let Some(o) = extra_opacity {
        entity.insert(o);
    }
    if let Some(radius) = def.backdrop_blur {
        entity.insert(crate::node::BackdropBlur { radius });
    }
    if let Some(i) = extra_interactable {
        entity.insert(i);
    }
    if let Some(b) = extra_bind {
        entity.insert(b);
    }
    if !def.bindings.is_empty() {
        entity.insert(crate::viewmodel::ViewBindings(
            def.bindings.iter().map(BindDef::to_bind).collect(),
        ));
    }
    if let Some(command) = &def.command {
        entity.insert(crate::viewmodel::UiCommandName(command.clone()));
    }
    if let Some((s, o)) = extra_scroll {
        entity.insert((s, o));
    }
    if let Some((l, c)) = extra_list {
        entity.insert((l, c));
    }
    if def.clip {
        entity.insert(ClipChildren);
    }
    // Group opacity, rounded clips and masks (ADR-2866).
    if let Some(opacity) = def.group_opacity {
        entity.insert(crate::group_layer::GroupOpacity(opacity));
    }
    match def.clip_shape {
        Some(ClipShapeDef::Rect) => {
            entity.insert(ClipChildren);
        }
        Some(ClipShapeDef::Rounded) => {
            entity.insert((ClipChildren, crate::group_layer::RoundedClip));
        }
        None => {}
    }
    if let Some(mask) = &def.mask {
        // `parse_ui_ron` holds a file to exactly one of the three.
        if let Some(asset) = &mask.asset {
            entity.insert(crate::group_layer::UiMaskAsset::new(asset.clone()).with_fit(mask.fit));
        } else if let Some(icon) = &mask.icon {
            entity.insert(crate::ui_image_assets::UiMaskIcon::new(icon.clone()).with_fit(mask.fit));
        } else if let Some(image) = mask.image {
            entity.insert(crate::group_layer::UiMask::new(image).with_fit(mask.fit));
        }
    }
    if def.disabled {
        entity.insert(Disabled);
    }
    if def.disabled_subtree {
        entity.insert(DisabledSubtree);
    }
    if let Some(pivot) = def.pivot.filter(|p| p[0].is_finite() && p[1].is_finite()) {
        entity.insert(crate::tween::UiTransform {
            pivot_ratio: pivot,
            ..crate::tween::UiTransform::IDENTITY
        });
    }
    if let Some(z) = def.z {
        entity.insert(ZOffset(z));
    }
    if def.safe_area {
        entity.insert(SafeAreaPadding);
    }
    for extra in extra_misc {
        extra.apply(&mut entity);
    }
    if !def.nav.is_empty() {
        entity.insert(DeclaredNav::new(def.nav.clone()));
    }
    entity.insert(ChildOf(parent));
    let entity = entity.id();
    match &def.repeat {
        // The children are the item template: the repeat pass spawns them per item.
        Some(repeat) => crate::repeat::make_host(world, entity, repeat, &def.children),
        None => match &def.widget {
            Some(widget @ (WidgetDef::Split(_) | WidgetDef::Foldable(_) | WidgetDef::Tabs(_))) => {
                spawn_container(world, entity, widget, &def.children);
            }
            _ => spawn_children(world, entity, &def.children),
        },
    }
    entity
}

/// The runtime spans of a text definition. With `markup` (v11 fold, ADR-7827) every literal span
/// is parsed by [`crate::text::parse_markup`] over its style; localization keys stay keys.
pub fn text_def_spans(t: &TextDef) -> Vec<TextSpan> {
    let mut spans = Vec::with_capacity(t.spans.len());
    for s in &t.spans {
        match s {
            TextSpanDef::Lit(text) if t.markup => {
                let parsed = crate::text::parse_markup(text, t.style.clone());
                if parsed.is_empty() {
                    // An empty or tag-only span keeps one empty span (bindings write into it).
                    spans.push(TextSpan::lit(String::new()).with_style(t.style.clone()));
                }
                spans.extend(parsed);
            }
            TextSpanDef::Lit(text) => spans.push(TextSpan::lit(text.clone()).with_style(t.style.clone())),
            TextSpanDef::Loc(LocSpan::Loc(key)) => {
                spans.push(TextSpan::loc(key.clone()).with_style(t.style.clone()))
            }
            TextSpanDef::Full { text, style } => match text.strip_prefix("loc:") {
                Some(key) => spans.push(TextSpan::loc(key.to_owned()).with_style(style.clone())),
                None if t.markup => spans.extend(crate::text::parse_markup(text, style.clone())),
                None => spans.push(TextSpan::lit(text.clone()).with_style(style.clone())),
            },
        }
    }
    spans
}

/// Spawns the children of a v11 container (ADR-7827) with the parts the runtime widget expects:
/// a split's divider between its first two children, a foldable's header before its body
/// children, a tab container's tab bar before its pages.
fn spawn_container(world: &mut World, entity: Entity, widget: &WidgetDef, children: &[UiNodeDef]) {
    match widget {
        WidgetDef::Split(d) => {
            for (i, child) in children.iter().enumerate() {
                if i == 1 {
                    spawn_split_divider(world, entity, d.axis);
                }
                spawn_node(world, entity, child);
            }
            if children.len() < 2 {
                // A split with fewer than two panes still gets its divider (the designer lints it).
                spawn_split_divider(world, entity, d.axis);
            }
        }
        WidgetDef::Foldable(d) => {
            let label = label_span(&d.title);
            let header = world
                .spawn((
                    UiNode,
                    NodeStyle::new(Style {
                        padding: crate::style::Edges::px(8.0),
                        align_items: crate::style::Align::Center,
                        ..Default::default()
                    }),
                    Interactable::default(),
                    crate::focus::Focusable::default(),
                    crate::default_widgets::FoldHeader,
                    Classes(vec!["button".to_owned(), "fold-header".to_owned()]),
                    Themed::default(),
                    ChildOf(entity),
                ))
                .id();
            world.spawn((
                UiNode,
                Text {
                    spans: vec![label],
                    wrap: WrapMode::None,
                    ..Default::default()
                },
                ChildOf(header),
            ));
            for child in children {
                let body = spawn_node(world, entity, child);
                let display = authored_style(child).display;
                world
                    .entity_mut(body)
                    .insert(crate::default_widgets::FoldBody { display });
                if !d.open
                    && let Some(mut style) = world.get_mut::<NodeStyle>(body)
                {
                    style.0.display = crate::style::Display::None;
                }
            }
        }
        WidgetDef::Tabs(d) => {
            world.spawn((
                UiNode,
                NodeStyle::new(Style {
                    direction: crate::style::FlexDirection::Row,
                    ..Default::default()
                }),
                TabBar {
                    tabs: d.tabs.clone(),
                    active: d.active,
                },
                Classes::one("tab-bar"),
                Themed::default(),
                ChildOf(entity),
            ));
            for (index, child) in children.iter().enumerate() {
                let page = spawn_node(world, entity, child);
                let display = authored_style(child).display;
                world
                    .entity_mut(page)
                    .insert(crate::default_widgets::TabPage { index, display });
                if index != d.active
                    && let Some(mut style) = world.get_mut::<NodeStyle>(page)
                {
                    style.0.display = crate::style::Display::None;
                }
            }
        }
        _ => spawn_children(world, entity, children),
    }
}

/// A literal label span, or a localized one for a `loc:` prefix.
fn label_span(text: &str) -> TextSpan {
    match text.strip_prefix("loc:") {
        Some(key) => TextSpan::loc(key.to_owned()),
        None => TextSpan::lit(text.to_owned()),
    }
}

fn spawn_split_divider(world: &mut World, split: Entity, axis: SplitAxisDef) {
    const THICKNESS: f32 = 6.0;
    world.spawn((
        UiNode,
        NodeStyle::new(Style {
            size: match axis {
                SplitAxisDef::Horizontal => [Dimension::Points(THICKNESS), Dimension::Auto],
                SplitAxisDef::Vertical => [Dimension::Auto, Dimension::Points(THICKNESS)],
            },
            justify_content: crate::style::JustifyContent::Center,
            ..Default::default()
        }),
        Interactable::default(),
        crate::default_widgets::SplitDivider,
        Classes::one("split-divider"),
        Themed::default(),
        ChildOf(split),
    ));
}

/// A node's declared focus neighbors (`.ui.ron` `nav`, ADR-7827), by node name, until
/// [`resolve_declared_nav`] writes them into the node's [`crate::Focusable`] links.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DeclaredNav {
    /// The declared names.
    pub def: NavDef,
    /// Whether every declared name was found (an unresolved name is retried when nodes spawn).
    pub resolved: bool,
}

impl DeclaredNav {
    /// Unresolved links for `def`.
    #[must_use]
    pub fn new(def: NavDef) -> Self {
        Self { def, resolved: false }
    }
}

/// The document root a node belongs to: the nearest ancestor spawned by [`spawn_ui`] (carrying
/// [`UiTemplate`]), else the topmost ancestor.
fn document_root(world: &World, mut entity: Entity) -> Entity {
    loop {
        if world.get::<UiTemplate>(entity).is_some() {
            return entity;
        }
        match world.get::<ChildOf>(entity) {
            Some(parent) => entity = parent.0,
            None => return entity,
        }
    }
}

/// The first node named `name` under `root`, depth first in child order (the designer's `#name`
/// rule).
fn find_named(world: &World, root: Entity, name: &str) -> Option<Entity> {
    let mut stack = vec![root];
    while let Some(e) = stack.pop() {
        if world.get::<NodeName>(e).is_some_and(|n| n.0 == name) {
            return Some(e);
        }
        if let Some(children) = world.get::<Children>(e) {
            let children: Vec<Entity> = children.iter().collect();
            stack.extend(children.into_iter().rev());
        }
    }
    None
}

/// The world change tick [`resolve_declared_nav`] last ran at: a node named or made focusable at
/// any point since (later in the same frame included) retries the unresolved declarations.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct DeclaredNavLastRun(pub Option<regia_ecs::ecs::change_detection::Tick>);

/// `Update`, before focus navigation: resolves every unresolved [`DeclaredNav`] (by name, inside
/// its document) into the node's [`crate::Focusable`] links. A node that is not focusable keeps
/// its declaration and gets no links (the designer lints it) and stays unresolved until it is.
/// Unresolved declarations retry when a named or focusable node was added since the last run
/// (by change tick, so a node spawned after this system in the same frame counts; review
/// sprint-w12-rev-g13 #1), so a steady screen costs two queries.
pub fn resolve_declared_nav(world: &mut World) {
    use regia_ecs::ecs::change_detection::{DetectChanges as _, Tick};
    let _span = tracing::trace_span!("ui_resolve_declared_nav").entered();
    let this_run = world.change_tick();
    let last_run = world
        .get_resource::<DeclaredNavLastRun>()
        .and_then(|r| r.0)
        .unwrap_or(Tick::new(0));
    let fresh = |added: Tick| added.is_newer_than(last_run, this_run);
    let spawned_names = world
        .query::<Ref<NodeName>>()
        .iter(world)
        .any(|n| fresh(n.added()));
    let spawned_focus = world
        .query::<Ref<crate::focus::Focusable>>()
        .iter(world)
        .any(|f| fresh(f.added()));
    let retry = spawned_names || spawned_focus;
    let pending: Vec<(Entity, NavDef)> = world
        .query::<(Entity, Ref<DeclaredNav>)>()
        .iter(world)
        .filter(|(_, d)| fresh(d.added()) || (!d.resolved && retry))
        .map(|(e, d)| (e, d.def.clone()))
        .collect();
    world.insert_resource(DeclaredNavLastRun(Some(this_run)));
    for (entity, def) in pending {
        let root = document_root(world, entity);
        let mut all = true;
        let mut links = crate::focus::NavLinks::default();
        for (dir, name) in def.entries() {
            let Some(name) = name else {
                continue;
            };
            let target = find_named(world, root, name);
            all &= target.is_some();
            match dir {
                "up" => links.up = target,
                "down" => links.down = target,
                "left" => links.left = target,
                _ => links.right = target,
            }
        }
        // Resolved only once the links are written: a node made focusable later retries.
        let written = match world.get_mut::<crate::focus::Focusable>(entity) {
            Some(mut focusable) => {
                focusable.nav = links;
                true
            }
            None => false,
        };
        if let Some(mut declared) = world.get_mut::<DeclaredNav>(entity) {
            declared.bypass_change_detection().resolved = all && written;
        }
    }
}

fn spawn_children(world: &mut World, parent: Entity, children: &[UiNodeDef]) {
    for def in children {
        spawn_node(world, parent, def);
    }
}

/// Late-added dynamic components (widget parts) applied via `EntityWorldMut`.
trait BundleLike {
    fn apply(self: Box<Self>, entity: &mut EntityWorldMut);
}

macro_rules! bundle_like {
    ($($t:ty),* $(,)?) => {
        $(
            impl BundleLike for $t {
                fn apply(self: Box<Self>, entity: &mut EntityWorldMut) {
                    // Move the bundle out of the box (works for non-Copy widget components).
                    entity.insert(*self);
                }
            }
        )*
    };
}

impl BundleLike for TabBar {
    fn apply(self: Box<Self>, entity: &mut EntityWorldMut) {
        entity.insert(*self);
    }
}

impl BundleLike for Tooltip {
    fn apply(self: Box<Self>, entity: &mut EntityWorldMut) {
        entity.insert(*self);
    }
}

impl BundleLike for crate::ui_image_assets::UiImageAsset {
    fn apply(self: Box<Self>, entity: &mut EntityWorldMut) {
        entity.insert(*self);
    }
}

bundle_like! {
    (Button, crate::focus::Focusable),
    (Toggle, crate::focus::Focusable),
    (Slider, crate::focus::Focusable),
    (ProgressBar,),
    (TextInput, crate::focus::Focusable),
    (Dropdown, crate::focus::Focusable),
    (TabBar,),
    (Modal, crate::focus::FocusScope),
    (Tooltip,),
    (crate::focus::Focusable,),
    (VirtualStick,),
    (VirtualButton,),
    (crate::menu::PopupMenu, crate::focus::Focusable),
    (crate::menu::ContextMenu,),
    (crate::menu::MenuBar,),
    (crate::game_widgets::LobbyList,),
    (crate::game_widgets::Scoreboard,),
    (crate::game_widgets::Radar,),
    (crate::game_widgets::MarkerLayer, crate::node::PickThrough),
    (crate::node::UiImageFit,),
    (crate::default_widgets::Split,),
    (crate::default_widgets::Foldable,),
    (crate::default_widgets::TabPages,),
}

/// One menu entry in `.ui.ron` (`popup_menu`, `context_menu`, `menu_bar`): an item, or a
/// separator with `separator: true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MenuEntryDef {
    /// Stable id (the agent path segment); empty uses the label.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Shown text.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// A separator line instead of an item (the other fields are ignored).
    #[serde(skip_serializing_if = "is_false")]
    pub separator: bool,
    /// Enabled (default `true`).
    #[serde(skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Activation flips `checked`.
    #[serde(skip_serializing_if = "is_false")]
    pub checkable: bool,
    /// Initial check state.
    #[serde(skip_serializing_if = "is_false")]
    pub checked: bool,
    /// Shortcut text shown right-aligned.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub shortcut: String,
    /// Submenu entries.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub submenu: Vec<MenuEntryDef>,
}

impl Default for MenuEntryDef {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            separator: false,
            enabled: true,
            checkable: false,
            checked: false,
            shortcut: String::new(),
            submenu: Vec::new(),
        }
    }
}

impl MenuEntryDef {
    /// The runtime item (a separator becomes an empty item; see [`Self::to_entry`]).
    pub fn to_item(&self) -> crate::menu::MenuItem {
        crate::menu::MenuItem {
            id: self.id.clone(),
            label: self.label.clone(),
            enabled: self.enabled,
            checkable: self.checkable,
            checked: self.checked,
            shortcut: self.shortcut.clone(),
            submenu: self.submenu.iter().map(Self::to_entry).collect(),
        }
    }

    /// The runtime entry.
    pub fn to_entry(&self) -> crate::menu::MenuEntry {
        if self.separator {
            crate::menu::MenuEntry::Separator
        } else {
            crate::menu::MenuEntry::Item(self.to_item())
        }
    }
}

fn default_true() -> bool {
    true
}

/// The canonical `.ui.ron` writer (ADR-1811): pretty RON with implicit `Some`, listing only the
/// fields that differ from their defaults (style fields included), so the text stays close to a
/// hand-written file. Lengths in points are written as bare numbers (`size: (200.0, Auto)`), as
/// authors write them. The result parses back to an equal [`UiFile`] ([`parse_ui_ron`]). This writer
/// knows nothing of a hand-written file's comments; [`crate::ron_merge::merge_ui_ron`] keeps them.
pub fn write_ui_ron(file: &UiFile) -> Result<String, crate::UiError> {
    let pretty = regia_core::ron_pretty()
        .indentor("    ")
        .struct_names(false)
        .separate_tuple_members(false)
        .enumerate_arrays(false);
    let mut text = ui_ron_options()
        .to_string_pretty(file, pretty)
        .map_err(|e| crate::UiError::Write(e.to_string()))?;
    text.push('\n');
    Ok(text)
}

/// Sparse [`Style`] serialization for [`write_ui_ron`]: only fields that differ from
/// `Style::default()`, with points lengths as bare numbers and uniform edges as one value (the
/// shorthands the `Style` deserializer accepts).
mod sparse_style {
    use serde::ser::{SerializeStruct, SerializeTuple};
    use serde::{Serialize, Serializer};

    use crate::style::{Dimension, Edges, Style};

    /// A length in its authoring form: a number for points, `Auto`, `Percent(p)`.
    pub(super) struct Dim(pub(super) Dimension);

    impl Serialize for Dim {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            match self.0 {
                Dimension::Points(p) => s.serialize_f32(p),
                Dimension::Auto => s.serialize_unit_variant("Dimension", 0, "Auto"),
                Dimension::Percent(p) => s.serialize_newtype_variant("Dimension", 2, "Percent", &p),
            }
        }
    }

    struct Pair([Dimension; 2]);

    /// Grid tracks as a plain list (the `SmallTracks` deserializer reads a sequence).
    struct Tracks<'a>(&'a [crate::style::TrackSizing]);

    impl Serialize for Tracks<'_> {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            self.0.serialize(s)
        }
    }

    impl Serialize for Pair {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            let mut t = s.serialize_tuple(2)?;
            t.serialize_element(&Dim(self.0[0]))?;
            t.serialize_element(&Dim(self.0[1]))?;
            t.end()
        }
    }

    struct EdgesOut(Edges);

    impl Serialize for EdgesOut {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            let e = self.0;
            if e.left == e.top && e.left == e.right && e.left == e.bottom {
                return Dim(e.left).serialize(s);
            }
            let mut t = s.serialize_tuple(4)?;
            for d in [e.left, e.top, e.right, e.bottom] {
                t.serialize_element(&Dim(d))?;
            }
            t.end()
        }
    }

    /// The field names this writer knows, in `Style`'s order (a test checks them against the
    /// deserializer's list, so a new `Style` field cannot be silently dropped).
    pub(super) const FIELDS: [&str; 23] = [
        "display",
        "position",
        "direction",
        "justify_content",
        "align_items",
        "align_self",
        "margin",
        "padding",
        "border",
        "inset",
        "size",
        "min_size",
        "max_size",
        "aspect_ratio",
        "overflow",
        "flex_wrap",
        "flex_grow",
        "flex_shrink",
        "flex_basis",
        "grid_rows",
        "grid_columns",
        "gap",
        "text_dir",
    ];

    pub(super) fn serialize<S: Serializer>(style: &Style, s: S) -> Result<S::Ok, S::Error> {
        let d = Style::default();
        let mut out = s.serialize_struct("Style", FIELDS.len())?;
        macro_rules! plain {
            ($($f:ident),*) => {$(
                if style.$f != d.$f {
                    out.serialize_field(stringify!($f), &style.$f)?;
                } else {
                    out.skip_field(stringify!($f))?;
                }
            )*};
        }
        macro_rules! wrapped {
            ($w:ident: $($f:ident),*) => {$(
                if style.$f != d.$f {
                    out.serialize_field(stringify!($f), &$w(style.$f))?;
                } else {
                    out.skip_field(stringify!($f))?;
                }
            )*};
        }
        plain!(
            display,
            position,
            direction,
            justify_content,
            align_items,
            align_self
        );
        wrapped!(EdgesOut: margin, padding, border, inset);
        wrapped!(Pair: size, min_size, max_size);
        plain!(aspect_ratio, overflow, flex_wrap, flex_grow, flex_shrink);
        wrapped!(Dim: flex_basis);
        for (name, tracks, default) in [
            ("grid_rows", &style.grid_rows, &d.grid_rows),
            ("grid_columns", &style.grid_columns, &d.grid_columns),
        ] {
            if tracks != default {
                out.serialize_field(name, &Tracks(&tracks.0))?;
            } else {
                out.skip_field(name)?;
            }
        }
        wrapped!(Pair: gap);
        plain!(text_dir);
        out.end()
    }
}

fn default_line_height() -> f32 {
    1.2
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A radar or marker layer anchored by hand (`position: Absolute` with an inset) and no size
    /// still gets its default size: with an automatic size it would lay out at zero and draw
    /// nothing. Only the marker layer's own position and inset defaults wait for a non-absolute
    /// node; an authored inset stays.
    #[test]
    fn an_absolute_game_widget_without_a_size_gets_its_default_size() {
        let mut absolute = Style {
            position: PositionType::Absolute,
            ..Style::default()
        };
        absolute.inset.right = Dimension::Points(12.0);
        absolute.inset.top = Dimension::Points(8.0);
        let radar = UiNodeDef {
            style: absolute.clone(),
            widget: Some(WidgetDef::Radar(RadarDef::default())),
            ..UiNodeDef::default()
        };
        let style = authored_style(&radar);
        assert_eq!(style.size, [Dimension::Points(200.0); 2]);
        assert_eq!(style.inset, absolute.inset, "the authored anchor stays");
        let layer = UiNodeDef {
            style: absolute.clone(),
            widget: Some(WidgetDef::MarkerLayer(MarkerLayerDef::default())),
            ..UiNodeDef::default()
        };
        let style = authored_style(&layer);
        assert_eq!(style.size, [Dimension::Percent(100.0); 2]);
        assert_eq!(style.inset, absolute.inset, "the authored anchor stays");
        // A flowing marker layer still becomes an absolute full-parent overlay.
        let flowing = UiNodeDef {
            widget: Some(WidgetDef::MarkerLayer(MarkerLayerDef::default())),
            ..UiNodeDef::default()
        };
        let style = authored_style(&flowing);
        assert_eq!(style.position, PositionType::Absolute);
        assert_eq!(style.size, [Dimension::Percent(100.0); 2]);
        // An absolute layer stretched between both insets of an axis keeps that axis automatic.
        let mut stretched = Style {
            position: PositionType::Absolute,
            ..Style::default()
        };
        stretched.inset = crate::style::Edges::all(Dimension::Points(0.0));
        stretched.inset.bottom = Dimension::Auto;
        let layer = UiNodeDef {
            style: stretched,
            widget: Some(WidgetDef::MarkerLayer(MarkerLayerDef::default())),
            ..UiNodeDef::default()
        };
        assert_eq!(
            authored_style(&layer).size,
            [Dimension::Auto, Dimension::Percent(100.0)]
        );
        // An authored size is kept as it is.
        let sized = UiNodeDef {
            style: Style {
                size: [Dimension::Points(64.0); 2],
                ..absolute
            },
            widget: Some(WidgetDef::Radar(RadarDef::default())),
            ..UiNodeDef::default()
        };
        assert_eq!(authored_style(&sized).size, [Dimension::Points(64.0); 2]);
    }

    const EXAMPLE: &str = r#"
(
    version: 1,
    root: (
        name: "hud",
        safe_area: true,
        style: (direction: column, padding: 16, gap: (8, 8)),
        children: [
            (
                name: "score",
                classes: ["surface"],
                style: (padding: 8, size: (200, Auto)),
                text: (spans: ["Score: 0"], style: (font_px: 22)),
                bind: (source: "Score.value", format: "Score: {:03}"),
            ),
            (
                widget: button,
                interactable: true,
                style: (padding: 10),
                children: [(
                    text: (spans: [Loc("menu-start")]),
                )],
            ),
        ],
    ),
)
"#;

    #[test]
    fn parses_example() {
        // EXAMPLE is a v1 file: the built-in step brings it to the current version.
        let file = parse_ui_ron(EXAMPLE, "test", &UiMigrators::default()).unwrap();
        assert_eq!(file.version, UI_RON_VERSION);
        assert_eq!(file.root.children.len(), 2);
        assert_eq!(file.root.children[0].bind.as_ref().unwrap().source, "Score.value");
        assert!(matches!(file.root.children[1].widget, Some(WidgetDef::Button)));
    }

    #[test]
    fn rejects_unknown_version() {
        let newer = UI_RON_VERSION + 1;
        let bad = EXAMPLE.replacen("version: 1", &format!("version: {newer}"), 1);
        let err = parse_ui_ron(&bad, "test", &UiMigrators::default()).unwrap_err();
        assert!(err.to_string().contains(&format!("version {newer}")), "{err}");
    }

    #[test]
    fn migrator_upgrades_old_versions() {
        let old = EXAMPLE.replacen("version: 1", "version: 0", 1);
        let mut migrators = UiMigrators::default();
        migrators.register(
            0,
            "test",
            Arc::new(|text| {
                *text = text.replacen("version: 0", "version: 1", 1);
                Ok(1)
            }),
        );
        // The game's v0 -> v1 step chains into the built-in v1 -> v2 step.
        let file = parse_ui_ron(&old, "test", &migrators).unwrap();
        assert_eq!(file.version, UI_RON_VERSION);
    }

    #[test]
    fn parses_menus() {
        let src = r#"
(
    version: 2,
    root: (
        children: [
            (
                name: "bar",
                widget: menu_bar([
                    (id: "file", label: "File", submenu: [
                        (id: "save", label: "Save", shortcut: "Ctrl+S"),
                        (separator: true),
                        (id: "grid", label: "Grid", checkable: true, checked: true),
                        (id: "quit", label: "Quit", enabled: false),
                    ]),
                ]),
            ),
            (name: "more", widget: popup_menu([(id: "a", label: "A")])),
            (name: "area", widget: context_menu([(label: "Copy")])),
        ],
    ),
)
"#;
        let file = parse_ui_ron(src, "test", &UiMigrators::default()).unwrap();
        let Some(WidgetDef::MenuBar(menus)) = &file.root.children[0].widget else {
            panic!("menu bar");
        };
        let file_menu = menus[0].to_item();
        assert_eq!(file_menu.submenu.len(), 4);
        assert_eq!(file_menu.submenu[1], crate::menu::MenuEntry::Separator);
        let grid = file_menu.submenu[2].item().unwrap();
        assert!(grid.checkable && grid.checked && grid.enabled);
        assert!(!file_menu.submenu[3].item().unwrap().enabled);
        assert_eq!(file_menu.submenu[0].item().unwrap().shortcut, "Ctrl+S");
        assert!(matches!(
            file.root.children[1].widget,
            Some(WidgetDef::PopupMenu(_))
        ));
        assert!(matches!(
            file.root.children[2].widget,
            Some(WidgetDef::ContextMenu(_))
        ));
        let bad = src.replacen("checkable: true", "checkabel: true", 1);
        assert!(parse_ui_ron(&bad, "test", &UiMigrators::default()).is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let bad = EXAMPLE.replacen("name: \"hud\",", "naem: \"hud\",", 1);
        assert!(parse_ui_ron(&bad, "test", &UiMigrators::default()).is_err());
    }

    /// ADR-1381 made the format v2 (menus), ADR-1811 v3 (view-model bindings), ADR-2045 v4
    /// (text effects), ADR-2129 v5 (`backdrop_blur`) and ADR-2489 v6 (`repeat`, `converter`);
    /// ADR-2866 v7 (group layers), ADR-3147 v8 (components), ADR-3148 v9 (named pictures), ADR-2958 v10 (game widgets) and
    /// ADR-9980 v11 (text input options, image fit); built-in steps cover v1 to v10, and `check-formats` sees them.
    #[test]
    fn v11_is_current_and_older_versions_have_builtin_steps() {
        assert_eq!(UI_RON_VERSION, 11);
        assert_eq!(ui_migration_sources(), vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        for from in 1..UI_RON_VERSION {
            assert!(
                ui_migration_sources().contains(&from),
                "no built-in step from v{from}"
            );
        }
    }

    /// The v1 → v6 upgrade changes only the root version number: comments, layout, a
    /// `version` inside a string or a comment and the node tree are kept byte for byte.
    #[test]
    fn upgrade_rewrites_only_the_root_version_number() {
        let v1 = r###"// version: 1 in a comment stays
UiFile( /* (version: 1) */
    root: (
        name: "version: 1",
        text: (spans: [r##"version: 1 "raw" "##]),
    ),
    version
        : 1, // trailing
)
"###;
        let v6 = upgrade_ui_ron(v1, "test").unwrap();
        assert_eq!(
            v6,
            v1.replacen(": 1, // trailing", ": 11, // trailing", 1),
            "{v6}"
        );
        let file = parse_ui_ron(&v6, "test", &UiMigrators::default()).unwrap();
        assert_eq!(file.version, 11);
        assert_eq!(file.root.name.as_deref(), Some("version: 1"));
    }

    /// Round trip: the upgraded text parses to the same tree as the v1 original, and upgrading is
    /// idempotent (a current file comes back unchanged).
    #[test]
    fn upgrade_round_trips_the_v1_example() {
        let v6 = upgrade_ui_ron(EXAMPLE, "test").unwrap();
        assert_eq!(v6, EXAMPLE.replacen("version: 1", "version: 11", 1));
        let from_v1 = parse_ui_ron(EXAMPLE, "test", &UiMigrators::default()).unwrap();
        let from_v6 = parse_ui_ron(&v6, "test", &UiMigrators::default()).unwrap();
        assert_eq!(from_v1, from_v6);
        assert_eq!(upgrade_ui_ron(&v6, "test").unwrap(), v6);
        // Every older version renumbers through the steps above it.
        for version in 2..=10 {
            let older = EXAMPLE.replacen("version: 1", &format!("version: {version}"), 1);
            assert_eq!(upgrade_ui_ron(&older, "test").unwrap(), v6, "v{version}");
        }
    }

    /// A newer file, a file with no root version and a v1 file whose body is invalid are errors,
    /// never a rewritten file.
    #[test]
    fn upgrade_rejects_newer_and_broken_files() {
        let next = UI_RON_VERSION + 1;
        let newer = EXAMPLE.replacen("version: 1", &format!("version: {next}"), 1);
        let err = upgrade_ui_ron(&newer, "test").unwrap_err();
        assert!(err.to_string().contains(&format!("version {next}")), "{err}");
        assert!(upgrade_ui_ron("(root: ())", "test").is_err());
        let typo = EXAMPLE.replacen("safe_area", "safe_aera", 1);
        assert!(upgrade_ui_ron(&typo, "test").is_err());
    }

    /// The version scanner finds only the root key, wherever it sits in the root struct.
    #[test]
    fn root_version_span_ignores_nested_strings_and_comments() {
        fn span(t: &str) -> Option<&str> {
            root_version_span(t).map(|(a, b)| &t[a..b])
        }
        assert_eq!(span("(version: 1, root: ())"), Some("1"));
        assert_eq!(span("(root: (name: \"version: 9\"), version:12)"), Some("12"));
        assert_eq!(span("/* ( */ UiFile(version /* c */ : 1)"), Some("1"));
        assert_eq!(span("(root: (version: 4))"), None);
        assert_eq!(span("(root: ()) // version: 1"), None);
        assert_eq!(span("(versions: 1)"), None);
        assert_eq!(
            span("#![enable(implicit_some)]\n(version: 8, root: ())"),
            Some("8")
        );
        assert_eq!(span("(r#\"version: 5\"#, version: 6)"), Some("6"));
        assert_eq!(span("('(', version: 7)"), Some("7"));
        assert_eq!(span(""), None);
        assert_eq!(span("(version: )"), None);
    }

    /// A game step from v0 chains into the built-in v1 → v2 step; a step that does not raise the
    /// version is an error instead of an endless loop; a version with no step names the gap.
    #[test]
    fn migrate_chains_and_rejects_steps_that_do_not_advance() {
        let mut migrators = UiMigrators::default();
        migrators.register(
            0,
            "stamp",
            Arc::new(|text| {
                *text = text.replacen("version: 0", "version: 1", 1);
                Ok(1)
            }),
        );
        let mut text = "(version: 0, root: ())".to_owned();
        assert_eq!(migrators.migrate(&mut text, 0), Ok(11));
        assert_eq!(text, "(version: 11, root: ())");

        let mut stuck = UiMigrators::default();
        stuck.register(0, "stuck", Arc::new(|_: &mut String| Ok(0)));
        let err = stuck.migrate(&mut "(version: 0)".to_owned(), 0).unwrap_err();
        assert!(err.contains("returned v0"), "{err}");

        let err = UiMigrators::default()
            .migrate(&mut "(version: 0)".to_owned(), 0)
            .unwrap_err();
        assert!(err.contains("no migrator from v0"), "{err}");
    }

    /// A game step registered for v1 is not used: the engine owns the v1 → v2 step.
    #[test]
    fn builtin_step_wins_over_a_registered_one() {
        let mut migrators = UiMigrators::default();
        migrators.register(
            1,
            "game",
            Arc::new(|_: &mut String| Err("game step ran".to_owned())),
        );
        let file = parse_ui_ron(EXAMPLE, "test", &migrators).unwrap();
        assert_eq!(file.version, UI_RON_VERSION);
    }

    /// The sparse style writer knows every `Style` field, in the deserializer's order, so a new
    /// field cannot be dropped by a designer save.
    #[test]
    fn sparse_style_covers_every_style_field() {
        let names = regia_core::schema::serde_field_names::<Style>();
        assert_eq!(names, sparse_style::FIELDS.map(str::to_owned).to_vec());
    }

    /// Every non-default style shape round-trips through the writer: percent and auto lengths,
    /// per-side edges, aspect ratio, grid tracks, gaps.
    #[test]
    fn write_round_trips_a_full_style() {
        use crate::style::{Dimension, Edges, TrackSizing};
        let mut root = UiNodeDef::default();
        root.style.size = [Dimension::Percent(50.0), Dimension::Auto];
        root.style.min_size = [Dimension::Points(10.5), Dimension::Percent(5.0)];
        root.style.margin = Edges {
            left: Dimension::Points(1.0),
            top: Dimension::Auto,
            right: Dimension::Percent(3.0),
            bottom: Dimension::Points(4.0),
        };
        root.style.padding = Edges::px(8.0);
        root.style.aspect_ratio = Some(1.5);
        root.style.display = crate::style::Display::Grid;
        root.style.grid_columns =
            crate::style::SmallTracks(vec![TrackSizing::Fr(1.0), TrackSizing::Px(40.0)]);
        root.style.gap = [Dimension::Points(2.0), Dimension::Points(3.0)];
        root.style.flex_grow = 1.0;
        root.bindings = vec![BindDef {
            source: "Vm.x".into(),
            target: BindTargetDef::Visible,
            mode: BindMode::OneTime,
            invert: true,
            format: Some("{:.1}".into()),
            converter: Some(BindConverter::MapRange {
                from: (0.0, 100.0),
                to: (0.0, 1.0),
                clamp: true,
            }),
            ..BindDef::default()
        }];
        root.repeat = Some(RepeatDef {
            source: "Vm.items".into(),
            key: Some("id".into()),
            ..RepeatDef::default()
        });
        root.command = Some("go".into());
        root.children = vec![UiNodeDef::default()];
        let file = UiFile {
            version: UI_RON_VERSION,
            component: None,
            root,
            variants: Vec::new(),
            back: None,
        };
        let text = write_ui_ron(&file).unwrap();
        let back =
            parse_ui_ron(&text, "written", &UiMigrators::default()).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert_eq!(back, file, "{text}");
        assert!(
            text.contains("padding: 8.0"),
            "uniform edges are one value: {text}"
        );
        assert!(
            !text.contains("flex_shrink"),
            "default fields are omitted: {text}"
        );
        let empty = write_ui_ron(&UiFile {
            version: UI_RON_VERSION,
            component: None,
            root: UiNodeDef::default(),
            variants: Vec::new(),
            back: None,
        })
        .unwrap();
        assert_eq!(
            parse_ui_ron(&empty, "e", &UiMigrators::default()).unwrap().root,
            UiNodeDef::default()
        );
    }

    /// A v1-labelled file that already uses the menu widgets (written by a build between the
    /// menus lane and the bump) still loads: v1 → v2 only renumbers.
    #[test]
    fn v1_file_with_menus_still_loads() {
        let src = "(version: 1, root: (widget: popup_menu([(id: \"a\", label: \"A\")])))";
        let file = parse_ui_ron(src, "test", &UiMigrators::default()).unwrap();
        assert!(matches!(file.root.widget, Some(WidgetDef::PopupMenu(_))));
    }

    /// A v10 file with a text input and images, comments and odd layout included.
    const V10_FIELDS: &str = r#"// A sign-in form (v10).
(
    version: 10, // the version line
    root: (
        children: [
            // The name field: placeholder and multi-line only.
            (name: "user", widget: text_input("Name", false)),
            (name: "logo", image: (id: 3)), // aspect kept by default
            (name: "flat", image: (id: 4, preserve_aspect: false)),
        ],
    ),
)
"#;

    /// `.ui.ron` v11 (ADR-9980): the v10 → v11 upgrade rewrites only the version number (comments
    /// and layout kept), and the old two-element `text_input` and an image without `fit` keep
    /// reading (no limits, no fit written).
    #[test]
    fn upgrade_v10_to_v11_keeps_comments_and_reads_old_text_input_and_image() {
        let v11 = upgrade_ui_ron(V10_FIELDS, "test").unwrap();
        assert_eq!(
            v11,
            V10_FIELDS.replacen("version: 10, //", "version: 11, //", 1),
            "{v11}"
        );
        let file = parse_ui_ron(&v11, "test", &UiMigrators::default()).unwrap();
        assert_eq!(file.version, 11);
        let kids = &file.root.children;
        assert_eq!(
            serde_json::to_value(&kids[0].widget).unwrap(),
            serde_json::json!({ "text_input": ["Name", false] }),
            "no options are written for the old form"
        );
        assert_eq!(
            serde_json::to_value(&kids[1].image).unwrap()["fit"],
            serde_json::Value::Null
        );
        assert_eq!(
            parse_ui_ron(V10_FIELDS, "test", &UiMigrators::default()).unwrap(),
            file
        );
    }

    /// `.ui.ron` v11 (ADR-9980): the text input options (`max_chars`, `secret`, `read_only`,
    /// `filter`) and the image `fit` parse, write back in the same short form and round-trip.
    #[test]
    fn authoring_v11_text_input_options_and_image_fit_round_trip() {
        let src = r#"(
    version: 11,
    root: (children: [
        (name: "pin", widget: text_input("PIN", false, (max_chars: 4, secret: '*', read_only: true, filter: integer))),
        (name: "num", widget: text_input("", true, (filter: numeric))),
        (name: "art", image: (id: 9, fit: cover)),
    ]),
)"#;
        let file = parse_ui_ron(src, "test", &UiMigrators::default()).unwrap();
        let kids = &file.root.children;
        assert_eq!(
            serde_json::to_value(&kids[0].widget).unwrap(),
            serde_json::json!({ "text_input": ["PIN", false, {
                "max_chars": 4, "secret": "*", "read_only": true, "filter": "integer"
            }] })
        );
        assert_eq!(
            serde_json::to_value(&kids[1].widget).unwrap(),
            serde_json::json!({ "text_input": ["", true, { "filter": "numeric" }] })
        );
        assert_eq!(serde_json::to_value(&kids[2].image).unwrap()["fit"], "cover");
        let text = write_ui_ron(&file).unwrap();
        assert!(text.contains("fit: cover"), "{text}");
        assert!(text.contains("max_chars: 4"), "{text}");
        assert_eq!(
            parse_ui_ron(&text, "back", &UiMigrators::default()).unwrap(),
            file
        );
        // `tile` is an image fit since the v11 fold (ADR-7751); an unknown fit or filter is refused.
        let tiled = parse_ui_ron(
            "(version: 11, root: (children: [(image: (id: 9, fit: tile))]))",
            "tile",
            &UiMigrators::default(),
        )
        .unwrap();
        assert_eq!(
            tiled.root.children[0].image.as_ref().map(ImageDef::effective_fit),
            Some(crate::node::ImageFit::Tile)
        );
        for bad in [
            "image: (id: 9, fit: squash)",
            "widget: text_input(\"\", false, (filter: hex))",
        ] {
            let src = format!("(version: 11, root: (children: [({bad})]))");
            assert!(
                parse_ui_ron(&src, "bad", &UiMigrators::default()).is_err(),
                "{bad}"
            );
        }
    }

    /// Audit rc1-sec-paths-z finding 7 (ADR-7388): the dev-build component reader (an
    /// [`regia_asset::AssetServer`] in the world) holds a component path from content to the same
    /// rule as the VFS branch, including a symlink inside `assets/` that leads out of it.
    #[test]
    fn the_asset_server_component_reader_stays_in_assets() {
        let base = regia_test_utils::unique_temp_dir("ui-component-reader");
        let root = base.join("project");
        std::fs::create_dir_all(root.join("assets/ui")).expect("assets");
        std::fs::create_dir_all(base.join("outside")).expect("outside");
        std::fs::write(root.join("assets/ui/bar.ui.ron"), "inside").expect("inside");
        std::fs::write(base.join("outside/secret.ui.ron"), "secret").expect("outside file");
        let mut world = World::new();
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: regia_asset::current_platform(),
            content_dir: None,
        })
        .expect("asset server");
        world.insert_resource(server);
        let mut read = world_component_reader(&world);
        assert_eq!(read("ui/bar.ui.ron").as_deref(), Ok("inside"));
        let outside = base.join("outside/secret.ui.ron").display().to_string();
        for bad in [
            "../../outside/secret.ui.ron",
            "ui/../../../outside/secret.ui.ron",
            "ui\\..\\..\\..\\outside\\secret.ui.ron",
            outside.as_str(),
            "C:\\outside\\secret.ui.ron",
            "\\\\server\\share\\secret.ui.ron",
        ] {
            let got = read(bad);
            assert!(
                got.as_deref() != Ok("secret") && got.is_err(),
                "{bad:?} read {got:?}"
            );
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(base.join("outside"), root.join("assets/linked")).expect("link");
            let got = read("linked/secret.ui.ron");
            assert!(got.is_err(), "a symlink out of assets/ was followed: {got:?}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The draw list of one `.ui.ron` v11 node written as `node`, under a 400 x 300 root, with a
    /// 32 x 32 image registered as id 7.
    fn draw_list_of(node: &str) -> Vec<crate::draw::UiQuad> {
        let src = format!("(version: 11, root: (children: [{node}]))");
        let file = parse_ui_ron(&src, "fit", &UiMigrators::default()).unwrap();
        let mut app = regia_app::App::new();
        app.add_plugins(crate::UiPlugin::code_only());
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        let world = app.world_mut();
        let root = world
            .spawn((
                UiRoot::default(),
                NodeStyle::new(Style {
                    size: [Dimension::Points(400.0), Dimension::Points(300.0)],
                    ..Default::default()
                }),
            ))
            .id();
        spawn_node(world, root, &file.root.children[0]);
        // A 32 x 32 picture under id 7, so the nine-slice has a pixel size to slice by.
        world
            .resource::<crate::render::SharedUiRenderer>()
            .lock()
            .queue_image_rgba(7, 32, 32, vec![255; 32 * 32 * 4]);
        app.update();
        app.update();
        app.world()
            .resource::<crate::render::SharedUiRenderer>()
            .lock()
            .quads()
            .to_vec()
    }

    /// ADR-7451 (6): `fit` does not apply to a nine-slice image (docs/formats/ui-ron.md): `fit:
    /// cover` with `nine_slice: (8, 8, 8, 8)` draws the same quads as without `fit`, while on a
    /// plain image of the same node `cover` does change them.
    #[test]
    fn image_fit_is_ignored_by_a_nine_slice_image() {
        let node = |image: &str| format!("(style: (size: (128, 64)), image: ({image}))");
        let sliced = draw_list_of(&node("id: 7, nine_slice: (8.0, 8.0, 8.0, 8.0)"));
        assert!(
            sliced.len() >= 9,
            "a nine-slice background draws its nine parts: {sliced:?}"
        );
        for fit in ["cover", "contain", "stretch"] {
            let fitted = draw_list_of(&node(&format!(
                "id: 7, fit: {fit}, nine_slice: (8.0, 8.0, 8.0, 8.0)"
            )));
            assert_eq!(fitted, sliced, "fit: {fit} changed a nine-slice image's quads");
        }
        // Control: on a plain image `cover` crops the picture, so the test can see a fit.
        let plain = draw_list_of(&node("id: 7, fit: stretch"));
        let covered = draw_list_of(&node("id: 7, fit: cover"));
        assert_ne!(plain, covered, "cover changes a plain image's quads");
    }
}
