// SPDX-License-Identifier: MPL-2.0
//! `ui.text_shape`: how a text node (or any string) shapes: its lines with their text and
//! direction, whether it was ellipsized, glyphs that drew `.notdef`, and the faces each run
//! used (book `agents/ui-text.md`, ADR-865).
//!
//! With `entity` it reports the node's current shape (what is on screen). With `text` it shapes
//! that string now with the given size, width, wrapping and overflow, without touching any node;
//! use it to check a translation fits a button or that a script has a face before shipping it.

use regia_ecs::prelude::*;
use regia_ui::text::{
    ShapedText, Text, TextContent, TextOverflow, TextStyle, TextSystem, WrapMode, shaped_string,
    text_paragraphs,
};
use regia_ui::{ComputedNode, Localization};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

/// Longest `text` the method shapes (characters).
const MAX_CHARS: usize = 4096;
/// Largest `font_px` and `width` accepted.
const MAX_FONT_PX: f64 = 512.0;
const MAX_WIDTH: f64 = 16384.0;

fn invalid(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn number(params: &Value, key: &str, code: &str, max: f64) -> Result<Option<f32>, RpcError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite() && *n > 0.0 && *n <= max)
            .map(|n| Some(n as f32))
            .ok_or_else(|| {
                invalid(
                    code,
                    format!("`{key}` must be a number above 0 and at most {max}"),
                    "sizes are in UI px",
                )
            }),
    }
}

fn wrap_param(params: &Value) -> Result<WrapMode, RpcError> {
    match params.get("wrap").filter(|v| !v.is_null()) {
        None => Ok(WrapMode::Word),
        Some(v) => match v.as_str() {
            Some("word") => Ok(WrapMode::Word),
            Some("glyph") => Ok(WrapMode::Glyph),
            Some("none") => Ok(WrapMode::None),
            _ => Err(invalid(
                "bad_wrap",
                "`wrap` must be \"word\", \"glyph\" or \"none\"",
                "word is the default",
            )),
        },
    }
}

fn overflow_param(params: &Value) -> Result<TextOverflow, RpcError> {
    match params.get("overflow").filter(|v| !v.is_null()) {
        None => Ok(TextOverflow::Visible),
        Some(v) => match v.as_str() {
            Some("visible") => Ok(TextOverflow::Visible),
            Some("clip") => Ok(TextOverflow::Clip),
            Some("ellipsis") => Ok(TextOverflow::Ellipsis),
            Some("scale_to_fit") => Ok(TextOverflow::ScaleToFit),
            _ => Err(invalid(
                "bad_overflow",
                "`overflow` must be \"visible\", \"clip\", \"ellipsis\" or \"scale_to_fit\"",
                "ellipsis applies to unwrapped text (wrap: \"none\") wider than `width`",
            )),
        },
    }
}

fn not_installed() -> RpcError {
    RpcError::new(
        UNSUPPORTED,
        "ui_not_installed",
        "this app has no UI text system",
        "the app needs regia_ui::UiPlugin",
    )
}

/// The UI content/locale cache key, kept private here to respect the lane's crate boundary.
/// Mirrors Regia's existing `regia_ui::text::text_digest`; the conformance test below compares
/// it with the public shaping system. Keep both in sync (ADR-4642).
fn text_digest(text: &Text, generation: u64) -> u64 {
    let mut h = fnv_offset();
    for span in &text.spans {
        match &span.content {
            TextContent::Lit(s) => {
                h = fnv_write_str(h, "L");
                h = fnv_write_str(h, s);
            }
            TextContent::Loc { key, args } => {
                h = fnv_write_str(h, "K");
                h = fnv_write_str(h, key);
                for (k, v) in args {
                    h = fnv_write_str(h, k);
                    h = fnv_write_str(h, v);
                }
            }
        }
        h = fnv_write_u32(h, span.style.font_px.to_bits());
        h = fnv_write_str(h, span.style.family.as_deref().unwrap_or(""));
        h = fnv_write_u32(h, (span.style.bold as u32) | ((span.style.italic as u32) << 1));
        // ADR-2045 shaping attributes, hashed only when set so default digests are unchanged.
        let st = &span.style;
        if st.letter_spacing != 0.0
            || st.weight.is_some()
            || st.underline
            || st.strikethrough
            || !st.features.is_empty()
        {
            h = fnv_write_str(h, "X");
            h = fnv_write_u32(h, st.letter_spacing.to_bits());
            h = fnv_write_u32(h, st.weight.map_or(0, u32::from));
            h = fnv_write_u32(h, (st.underline as u32) | ((st.strikethrough as u32) << 1));
            for f in &st.features {
                h = fnv_write_str(h, f);
                h = fnv_write_str(h, ";");
            }
        }
    }
    h = fnv_write_u32(h, text.align as u32);
    h = fnv_write_u32(h, text.wrap as u32);
    h = fnv_write_u32(h, text.overflow as u32);
    h = fnv_write_u32(h, text.line_height.to_bits());
    h = fnv_write_u64(h, generation);
    h
}

fn fnv_offset() -> u64 {
    0xcbf29ce484222325
}

fn fnv_write_u32(h: u64, v: u32) -> u64 {
    (h ^ u64::from(v)).wrapping_mul(0x100000001b3)
}

fn fnv_write_u64(h: u64, v: u64) -> u64 {
    (h ^ v).wrapping_mul(0x100000001b3)
}

fn fnv_write_str(mut h: u64, s: &str) -> u64 {
    for b in s.as_bytes() {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    h
}

/// The report for one shape of `text`.
fn report(sys: &TextSystem, text: &Text, shaped: &ShapedText, loc: &Localization) -> Value {
    let string = shaped_string(text, loc);
    let paragraphs = text_paragraphs(&string);
    let lines: Vec<Value> = shaped
        .lines
        .iter()
        .map(|l| {
            let text = paragraphs
                .get(l.paragraph as usize)
                .and_then(|p| p.get(l.range[0] as usize..l.range[1] as usize))
                .unwrap_or("");
            json!({
                "text": text,
                "rtl": l.rtl,
                "width": l.width,
                "baseline": l.baseline,
                "glyphs": l.glyph_count,
                "ellipsized": l.ellipsized,
            })
        })
        .collect();
    let mut fonts: Vec<String> = Vec::new();
    for glyph in &shaped.glyphs {
        let name = sys
            .font_system
            .db()
            .face(glyph.cache_key.font_id)
            .and_then(|f| f.families.first().map(|(n, _)| n.clone()))
            .unwrap_or_default();
        if !fonts.contains(&name) {
            fonts.push(name);
        }
    }
    json!({
        "text": string,
        "size": [shaped.size[0], shaped.size[1]],
        "lines": lines,
        "glyphs": shaped.glyphs.len(),
        "missing_glyphs": shaped.missing_glyphs,
        "truncated": shaped.truncated,
        "fonts": fonts,
        "default_family": sys.default_family(),
    })
}

/// `ui.text_shape { entity | text, font_px?, width?, wrap?, overflow?, family?, bold? }`.
pub(crate) fn text_shape(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !world.contains_resource::<TextSystem>() {
        return Err(not_installed());
    }
    let entity = match params
        .get("entity")
        .or_else(|| params.get("node"))
        .filter(|v| !v.is_null())
    {
        None => None,
        Some(v) => Some(v.as_u64().and_then(Entity::try_from_bits).ok_or_else(|| {
            invalid(
                "bad_entity",
                "`entity` must be the node id from ui.query",
                "ui.query { text: \"...\" } lists node ids",
            )
        })?),
    };
    if let Some(entity) = entity {
        let Ok(node) = world.get_entity(entity) else {
            return Err(RpcError::new(
                NOT_FOUND,
                "unknown_entity",
                "the entity does not exist",
                "take entity ids from a ui.query result",
            ));
        };
        let Some(text) = node.get::<Text>().cloned() else {
            return Err(RpcError::new(
                NOT_FOUND,
                "not_a_text",
                "the entity has no Text",
                "query text nodes with ui.query { text: \"...\" }",
            ));
        };
        let Some(shaped) = node.get::<ShapedText>().cloned() else {
            return Err(invalid(
                "text_not_shaped",
                "the text has not been shaped yet",
                "step a frame (play.step) and ask again",
            ));
        };
        let scale = node.get::<ComputedNode>().map_or(1.0, |c| c.scale);
        let fallback = Localization::default();
        let loc = world.get_resource::<Localization>().unwrap_or(&fallback);
        if shaped.digest != text_digest(&text, loc.generation.0) {
            return Err(invalid(
                "text_not_shaped",
                "the text or locale changed since shaping",
                "step a frame (play.step) and ask again",
            ));
        }
        let sys = world.resource::<TextSystem>();
        let mut out = report(sys, &text, &shaped, loc);
        out["entity"] = json!(entity.to_bits());
        out["scale"] = json!(scale);
        return Ok(out);
    }

    let Some(string) = params.get("text").filter(|v| !v.is_null()) else {
        return Err(invalid(
            "ui_text_missing",
            "pass `entity` (a text node) or `text` (a string to shape)",
            "ui.text_shape { text: \"Start game\", width: 120 }",
        ));
    };
    let Some(string) = string.as_str() else {
        return Err(invalid(
            "bad_text",
            "`text` must be a string",
            "pass the string to shape",
        ));
    };
    if string.chars().count() > MAX_CHARS {
        return Err(invalid(
            "text_too_long",
            format!("`text` is longer than {MAX_CHARS} characters"),
            "shape one label or paragraph at a time",
        ));
    }
    let font_px = number(params, "font_px", "bad_font_px", MAX_FONT_PX)?.unwrap_or(18.0);
    let width = number(params, "width", "bad_width", MAX_WIDTH)?;
    let wrap = wrap_param(params)?;
    let overflow = overflow_param(params)?;
    let family = match params.get("family").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    invalid(
                        "bad_family",
                        "`family` must be a font family name",
                        "e.g. \"Inter\"",
                    )
                })?
                .to_owned(),
        ),
    };
    let bold = match params.get("bold").filter(|v| !v.is_null()) {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| invalid("bad_bold", "`bold` must be true or false", "pass a JSON boolean"))?,
    };
    let text = Text::new(string)
        .with_style(TextStyle {
            font_px,
            bold,
            family,
            ..TextStyle::default()
        })
        .with_wrap(wrap)
        .with_overflow(overflow);
    Ok(world.resource_scope(|world, mut sys: Mut<TextSystem>| {
        let fallback = Localization::default();
        let loc = world.get_resource::<Localization>().unwrap_or(&fallback);
        let shaped = sys.shape(&text, 1.0, width, loc);
        report(&sys, &text, &shaped, loc)
    }))
}

/// Changes the default face of unpinned UI text; loaded family validation is engine-owned.
pub(crate) fn set_default_font(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let family = match params.get("family") {
        Some(Value::Null) => None,
        Some(Value::String(name)) if !name.is_empty() => Some(name.as_str()),
        _ => {
            return Err(invalid(
                "bad_family",
                "family must be a nonempty string or null",
                "null restores embedded Inter; load a project font before selecting its family",
            ));
        }
    };
    let mut fonts = world.get_resource_mut::<TextSystem>().ok_or_else(not_installed)?;
    let changed = fonts.set_default_family(family).map_err(|error| {
        invalid(
            "bad_family",
            error.to_string(),
            "select a loaded font family, or null to restore Inter",
        )
    })?;
    Ok(json!({"family": fonts.default_family(), "changed": changed}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_digest_matches_the_ui_shaping_system() {
        use regia_ecs::ecs::system::RunSystemOnce;
        use regia_ui::text::{TextAlign, TextSpan};

        let mut texts = vec![
            Text::default(),
            Text::new("Hi"),
            Text {
                spans: vec![
                    TextSpan::lit("Hello "),
                    TextSpan {
                        content: TextContent::Loc {
                            key: "greeting".into(),
                            args: vec![("speaker".into(), "Ranger".into()), ("count".into(), "2".into())],
                        },
                        style: TextStyle::default(),
                    },
                ],
                ..Text::default()
            },
        ];
        for style in [
            TextStyle {
                font_px: 22.0,
                family: Some("Inter".into()),
                bold: true,
                italic: true,
                ..TextStyle::default()
            },
            TextStyle {
                letter_spacing: 0.08,
                weight: Some(550),
                underline: true,
                strikethrough: true,
                features: vec!["liga=0".into(), "tnum".into()],
                ..TextStyle::default()
            },
            TextStyle {
                color: regia_ui::UiColor([0.2, 0.4, 0.6, 1.0]),
                ..TextStyle::default()
            },
        ] {
            texts.push(Text::new("Styled").with_style(style));
        }
        for align in [TextAlign::Start, TextAlign::Center, TextAlign::End] {
            for wrap in [WrapMode::Word, WrapMode::Glyph, WrapMode::None] {
                for overflow in [
                    TextOverflow::Visible,
                    TextOverflow::Clip,
                    TextOverflow::Ellipsis,
                    TextOverflow::ScaleToFit,
                ] {
                    let mut text = Text::new("Layout").with_wrap(wrap).with_overflow(overflow);
                    text.align = align;
                    text.line_height = 1.6;
                    texts.push(text);
                }
            }
        }
        let mut world = world();
        for generation in [0, 7, u64::MAX] {
            world.resource_mut::<Localization>().generation.0 = generation;
            for text in &texts {
                let entity = world.spawn(text.clone()).id();
                world
                    .run_system_once(regia_ui::text::shape_text)
                    .expect("UI shape");
                assert_eq!(
                    world.get::<ShapedText>(entity).expect("shape").digest,
                    text_digest(text, generation),
                    "{text:?}, generation {generation}"
                );
                assert!(world.despawn(entity));
            }
        }
    }

    #[test]
    fn stale_shape_is_not_sliced_into_the_new_string() {
        use regia_ecs::ecs::system::RunSystemOnce;
        let mut world = world();
        let node = world.spawn(Text::new("Hi")).id();
        world
            .run_system_once(regia_ui::text::shape_text)
            .expect("shape original");
        let params = json!({ "entity": node.to_bits() });
        let original = text_shape(&mut world, &params).expect("current shape");
        assert_eq!(original["lines"][0]["text"], "Hi");
        for text in ["Hello, world", "H", "é"] {
            world.entity_mut(node).insert(Text::new(text));
            let err = text_shape(&mut world, &params).expect_err("stale shape");
            assert_eq!(code(&err), "text_not_shaped");
        }
        world
            .run_system_once(regia_ui::text::shape_text)
            .expect("refresh");
        let current = text_shape(&mut world, &params).expect("refreshed shape");
        assert_eq!(current["lines"][0]["text"], "é");
        world.resource_mut::<Localization>().generation.0 += 1;
        let err = text_shape(&mut world, &params).expect_err("locale generation changed");
        assert_eq!(code(&err), "text_not_shaped");
    }

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(TextSystem::embedded_only());
        world.init_resource::<Localization>();
        world
    }

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or("")
            .to_owned()
    }

    #[test]
    fn shapes_a_string_and_reports_lines_and_faces() {
        let mut world = world();
        let out = text_shape(&mut world, &json!({ "text": "Hello\n\u{4F60}\u{597D}" })).expect("shape");
        assert_eq!(out["lines"].as_array().map(Vec::len), Some(2));
        assert_eq!(out["lines"][0]["text"], json!("Hello"));
        assert_eq!(out["lines"][1]["text"], json!("\u{4F60}\u{597D}"));
        assert_eq!(out["missing_glyphs"], json!(0));
        assert_eq!(out["truncated"], json!(false));
        let fonts: Vec<&str> = out["fonts"]
            .as_array()
            .expect("fonts")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            fonts.contains(&"Inter") && fonts.contains(&"Noto Sans SC"),
            "{fonts:?}"
        );
    }

    #[test]
    fn reports_ellipsis_and_missing_glyphs() {
        let mut world = world();
        let out = text_shape(
            &mut world,
            &json!({ "text": "A long label that cannot fit", "width": 60, "wrap": "none", "overflow": "ellipsis" }),
        )
        .expect("shape");
        assert_eq!(out["truncated"], json!(true));
        assert_eq!(out["lines"][0]["ellipsized"], json!(true));
        let thai = text_shape(&mut world, &json!({ "text": "\u{1200}" })).expect("shape");
        assert!(thai["missing_glyphs"].as_u64().unwrap_or(0) >= 1);
    }

    #[test]
    fn reports_a_node_and_validates_params() {
        use regia_ecs::ecs::system::RunSystemOnce;
        let mut world = world();
        let node = world.spawn(Text::new("Play")).id();
        world
            .run_system_once(regia_ui::text::shape_text)
            .expect("shape node");
        let out = text_shape(&mut world, &json!({ "entity": node.to_bits() })).expect("node");
        assert_eq!(out["lines"][0]["text"], json!("Play"));
        assert_eq!(out["entity"], json!(node.to_bits()));

        let bare = world.spawn_empty().id();
        let err = text_shape(&mut world, &json!({ "entity": bare.to_bits() })).expect_err("no text");
        assert_eq!(code(&err), "not_a_text");
        let unshaped = world.spawn(Text::new("later")).id();
        let err = text_shape(&mut world, &json!({ "entity": unshaped.to_bits() })).expect_err("unshaped");
        assert_eq!(code(&err), "text_not_shaped");
        for (params, expected) in [
            (json!({}), "ui_text_missing"),
            (json!({ "text": 5 }), "bad_text"),
            (json!({ "text": "x", "width": -1 }), "bad_width"),
            (json!({ "text": "x", "font_px": 0 }), "bad_font_px"),
            (json!({ "text": "x", "wrap": "wide" }), "bad_wrap"),
            (json!({ "text": "x", "overflow": "fade" }), "bad_overflow"),
            (json!({ "text": "x", "family": "" }), "bad_family"),
            (json!({ "text": "x", "bold": "yes" }), "bad_bold"),
            (json!({ "entity": "nope" }), "bad_entity"),
            (json!({ "text": "x".repeat(MAX_CHARS + 1) }), "text_too_long"),
        ] {
            let err = text_shape(&mut world, &params).expect_err("invalid");
            assert_eq!(code(&err), expected, "{params}");
        }
        let mut empty = World::new();
        let err = text_shape(&mut empty, &json!({ "text": "x" })).expect_err("no ui");
        assert_eq!(code(&err), "ui_not_installed");
    }
}
