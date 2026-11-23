// SPDX-License-Identifier: MPL-2.0
//! `ui.key`, `ui.text_state`, `ui.text_select`: drive and read runtime-UI text fields (book
//! `agents/ui-text-input.md`; ADR-273 W01 part 1, ADR-336).
//!
//! `ui.key` writes real keyboard messages (modifiers down, the key, modifiers up, in order), so a
//! chord reaches the focused widget exactly as a keyboard would deliver it, on the next update.
//! The other two read and set a `TextInput`'s caret and selection at once. Positions in this
//! protocol are character (Unicode scalar) indices, not bytes.

use regia_app::prelude::*;
use regia_ui::focus::FocusState;
use regia_ui::line_break_sa::with_sa_breaks;
use regia_ui::node::NodeName;
use regia_ui::{TextInput, focus_node};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Most chords one `ui.key` call may send.
const MAX_CHORDS: usize = 64;
/// Longest `text` `ui.text_breaks` segments (characters).
const MAX_BREAK_CHARS: usize = 4096;

/// An error with a stable snake_case `data.reason` (documented in the book page).
fn error(code: i64, reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let regia_code = if code == NOT_FOUND { "F0003" } else { "F0002" };
    RpcError {
        code,
        message: message.into(),
        data: Some(json!({ "code": regia_code, "reason": reason, "hint": hint })),
    }
}

/// Resolves the target field: `node` (entity bits), else `name` (exact `NodeName`), else the
/// focused node. It must carry a `TextInput`.
fn target(world: &mut World, params: &Value) -> Result<Entity, RpcError> {
    let entity = if let Some(bits) = params.get("node").filter(|v| !v.is_null()) {
        let entity = bits.as_u64().and_then(Entity::try_from_bits).ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "ui_bad_param",
                "`node` must be a node id",
                "take ids from ui.query",
            )
        })?;
        if world.get_entity(entity).is_err() {
            return Err(error(
                NOT_FOUND,
                "ui_node_not_found",
                "no node has that id",
                "take ids from ui.query",
            ));
        }
        entity
    } else if let Some(name) = params.get("name").filter(|v| !v.is_null()) {
        let name = name.as_str().ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "ui_bad_param",
                "`name` must be a string",
                "the node's NodeName",
            )
        })?;
        let mut q = world.query::<(Entity, &NodeName)>();
        let found: Vec<Entity> = q
            .iter(world)
            .filter(|(_, n)| n.0 == name)
            .map(|(e, _)| e)
            .collect();
        match found.as_slice() {
            [one] => *one,
            [] => {
                return Err(error(
                    NOT_FOUND,
                    "ui_node_not_found",
                    format!("no node is named `{name}`"),
                    "list names with ui.query",
                ));
            }
            _ => {
                return Err(error(
                    INVALID_PARAMS,
                    "ui_node_ambiguous",
                    format!("{} nodes are named `{name}`", found.len()),
                    "pass the `node` id from ui.query",
                ));
            }
        }
    } else {
        world
            .get_resource::<FocusState>()
            .and_then(|f| f.focused)
            .ok_or_else(|| {
                error(
                    INVALID_PARAMS,
                    "ui_no_focus",
                    "nothing has focus",
                    "pass `node` or `name`, or focus a field with ui.focus",
                )
            })?
    };
    if world.get::<TextInput>(entity).is_none() {
        return Err(error(
            INVALID_PARAMS,
            "ui_not_text_input",
            "the node is not a text field",
            "ui.query { type: \"TextInput\" } lists the text fields",
        ));
    }
    Ok(entity)
}

fn chars_before(text: &str, byte: usize) -> usize {
    text.get(..byte)
        .map_or_else(|| text.chars().count(), |s| s.chars().count())
}

fn byte_of(text: &str, chars: usize) -> usize {
    text.char_indices().nth(chars).map_or(text.len(), |(i, _)| i)
}

fn state(world: &World, entity: Entity) -> Value {
    let Some(input) = world.get::<TextInput>(entity) else {
        return Value::Null;
    };
    let focused = world
        .get_resource::<FocusState>()
        .is_some_and(|f| f.focused == Some(entity));
    let (a, b) = input.selection();
    let text = &input.text;
    let shown = if input.secret.is_some() {
        input.display_text()
    } else {
        text.clone()
    };
    let before_caret = text.get(..input.caret).unwrap_or("");
    let line = before_caret.matches('\n').count();
    let column = before_caret
        .rsplit_once('\n')
        .map_or(before_caret, |(_, tail)| tail)
        .chars()
        .count();
    let selected = if input.secret.is_some() {
        Value::Null
    } else {
        json!(input.selected_text())
    };
    json!({
        "node": entity.to_bits(),
        "focused": focused,
        "text": shown,
        "length": text.chars().count(),
        "caret": chars_before(text, input.caret),
        "anchor": chars_before(text, input.selection_anchor),
        "selection": [chars_before(text, a), chars_before(text, b)],
        "selected_text": selected,
        "line": line,
        "column": column,
        "multiline": input.multiline,
        "read_only": input.read_only,
        "secret": input.secret.is_some(),
        "filter": input.filter.name(),
        "max_chars": input.max_chars,
        "placeholder": input.placeholder,
        "scroll": input.scroll,
        "preedit": input.preedit,
        "preedit_cursor": input
            .preedit_cursor
            .map(|(a, b)| [chars_before(&input.preedit, a), chars_before(&input.preedit, b)]),
        "ime_rect": input.ime_rect(),
        "visual_caret": input
            .visual_caret()
            .map(|[x, y, height]| json!({ "x": x, "y": y, "height": height })),
        "can_undo": input.can_undo(),
        "can_redo": input.can_redo(),
    })
}

/// `ui.text_state { node?, name? }`: the field's content, caret, selection and history.
pub(crate) fn text_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = target(world, params)?;
    Ok(state(world, entity))
}

/// `ui.text_select { node?, name?, anchor?, caret }`: sets the selection (character indices).
pub(crate) fn text_select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = target(world, params)?;
    let index = |key: &str| -> Result<Option<usize>, RpcError> {
        match params.get(key).filter(|v| !v.is_null()) {
            None => Ok(None),
            Some(v) => v
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .map(Some)
                .ok_or_else(|| {
                    error(
                        INVALID_PARAMS,
                        "ui_bad_param",
                        format!("`{key}` must be a character index (0 or more)"),
                        "indices count characters, not bytes",
                    )
                }),
        }
    };
    let caret = index("caret")?.ok_or_else(|| {
        error(
            INVALID_PARAMS,
            "ui_bad_param",
            "`caret` is required",
            "ui.text_select { caret: 5 } moves the caret; add `anchor` to select",
        )
    })?;
    let anchor = index("anchor")?.unwrap_or(caret);
    if let Some(mut input) = world.get_mut::<TextInput>(entity) {
        let (a, c) = (byte_of(&input.text, anchor), byte_of(&input.text, caret));
        input.select(a, c);
    }
    Ok(state(world, entity))
}

/// `ui.text_breaks { text, locale? }`: the dictionary word boundaries UAX #14 class SA scripts
/// (Thai, Lao, Khmer, Myanmar — words written without spaces) wrap at. `breaks` lists the
/// character offsets between words (like `ui.text_state`, positions count characters, not
/// bytes); `words` lists the words themselves. Both are empty for text without SA characters
/// (Latin, CJK and spaced scripts break under their normal UAX #14 rules). The same
/// segmentation drives UI text wrapping, captions and word-wise caret movement (ADR-3323).
pub(crate) fn text_breaks(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _ = world;
    let Some(text) = params.get("text").filter(|v| !v.is_null()) else {
        return Err(error(
            INVALID_PARAMS,
            "ui_text_missing",
            "pass `text`: the string to segment",
            "ui.text_breaks { text: \"ฉันกินข้าว\" }",
        ));
    };
    let Some(text) = text.as_str() else {
        return Err(error(
            INVALID_PARAMS,
            "bad_text",
            "`text` must be a string",
            "pass the string to segment",
        ));
    };
    if text.chars().count() > MAX_BREAK_CHARS {
        return Err(error(
            INVALID_PARAMS,
            "text_too_long",
            format!("`text` is longer than {MAX_BREAK_CHARS} characters"),
            "segment one label or paragraph at a time",
        ));
    }
    let locale = match params.get("locale").filter(|v| !v.is_null()) {
        None => Value::Null,
        Some(v) => {
            let Some(tag) = v.as_str() else {
                return Err(error(
                    INVALID_PARAMS,
                    "bad_locale",
                    "`locale` must be a BCP 47 tag",
                    "the engine detects the script from the text; `locale` is informational",
                ));
            };
            // The segmentation is script-based (the text's own characters), so any tag is
            // accepted and echoed back; it selects nothing yet.
            json!(tag)
        }
    };
    let sa_text = regia_ui::line_break_sa::has_sa(text);
    let mut breaks: Vec<usize> = Vec::new();
    let mut words: Vec<&str> = Vec::new();
    with_sa_breaks(text, |sa| {
        let mut last = 0;
        for &b in sa {
            breaks.push(chars_before(text, b));
            words.push(&text[last..b]);
            last = b;
        }
        if sa_text && last < text.len() {
            words.push(&text[last..]);
        }
    });
    Ok(json!({
        "text": text,
        "locale": locale,
        "breaks": breaks,
        "words": words,
    }))
}

/// Splits `"Ctrl+Shift+Left"` into platform modifier key names and the key.
fn parse_chord(chord: &str) -> Result<(Vec<&'static str>, String), RpcError> {
    let bad = |why: String| {
        error(
            INVALID_PARAMS,
            "ui_bad_key",
            why,
            "chords look like \"Ctrl+Shift+Left\", \"Backspace\", \"a\" or \"Ctrl++\"",
        )
    };
    let (mods_part, key) = if chord == "+" {
        ("", "+")
    } else if let Some(head) = chord.strip_suffix("++") {
        (head, "+")
    } else {
        match chord.rsplit_once('+') {
            Some((head, key)) => (head, key),
            None => ("", chord),
        }
    };
    if key.is_empty() {
        return Err(bad(format!("`{chord}` names no key")));
    }
    let mut mods = Vec::new();
    for m in mods_part.split('+').filter(|m| !m.is_empty()) {
        let name = match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "Control",
            "shift" => "Shift",
            "alt" | "option" => "Alt",
            "meta" | "cmd" | "command" | "super" => "Super",
            other => {
                return Err(bad(format!(
                    "`{other}` is not a modifier (Ctrl, Shift, Alt, Meta)"
                )));
            }
        };
        mods.push(name);
    }
    // Friendly names for the keys the text field and focus navigation read.
    let key = match key.to_ascii_lowercase().as_str() {
        "left" => "ArrowLeft".to_owned(),
        "right" => "ArrowRight".to_owned(),
        "up" => "ArrowUp".to_owned(),
        "down" => "ArrowDown".to_owned(),
        "esc" => "Escape".to_owned(),
        "del" => "Delete".to_owned(),
        "return" => "Enter".to_owned(),
        " " => "Space".to_owned(),
        _ => key.to_owned(),
    };
    Ok((mods, key))
}

fn send_key(world: &mut World, window: Entity, key: &str, pressed: bool) {
    let state = if pressed {
        regia_platform::ButtonState::Pressed
    } else {
        regia_platform::ButtonState::Released
    };
    world.write_message(regia_platform::KeyboardInput {
        window,
        physical_key: key.to_owned(),
        logical_key: key.to_owned(),
        state,
        repeat: false,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

/// `ui.key { keys, node?, name? }`: presses each chord in order, in one frame.
pub(crate) fn key(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let keys: Vec<&str> = match params.get("keys") {
        Some(Value::Array(list)) => list
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                error(
                    INVALID_PARAMS,
                    "ui_bad_param",
                    "`keys` must be a list of chord strings",
                    "e.g. [\"Ctrl+A\", \"Backspace\"]",
                )
            })?,
        Some(Value::String(one)) => vec![one.as_str()],
        _ => {
            return Err(error(
                INVALID_PARAMS,
                "ui_key_empty",
                "give `keys`: a chord or a list of chords",
                "ui.key { keys: [\"Ctrl+Shift+Left\", \"Ctrl+C\"] }",
            ));
        }
    };
    if keys.is_empty() {
        return Err(error(
            INVALID_PARAMS,
            "ui_key_empty",
            "`keys` is empty",
            "ui.key { keys: [\"Enter\"] }",
        ));
    }
    if keys.len() > MAX_CHORDS {
        return Err(error(
            INVALID_PARAMS,
            "ui_bad_param",
            format!("at most {MAX_CHORDS} chords per call"),
            "split long sequences over several calls",
        ));
    }
    let chords = keys
        .iter()
        .map(|k| parse_chord(k))
        .collect::<Result<Vec<_>, _>>()?;
    let named =
        params.get("node").is_some_and(|v| !v.is_null()) || params.get("name").is_some_and(|v| !v.is_null());
    let entity = if named {
        let entity = target(world, params)?;
        focus_node(world, entity);
        Some(entity)
    } else {
        world.get_resource::<FocusState>().and_then(|f| f.focused)
    };
    let window = world
        .query_filtered::<Entity, With<regia_platform::PrimaryWindow>>()
        .iter(world)
        .next()
        .unwrap_or(Entity::PLACEHOLDER);
    for (mods, key) in &chords {
        for m in mods {
            send_key(world, window, m, true);
        }
        send_key(world, window, key, true);
        send_key(world, window, key, false);
        for m in mods.iter().rev() {
            send_key(world, window, m, false);
        }
    }
    Ok(json!({ "node": entity.map(Entity::to_bits), "sent": chords.len(), "applies": "next update" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with_field(text: &str) -> (World, Entity) {
        let mut world = World::new();
        world.init_resource::<FocusState>();
        world.init_resource::<Messages<regia_platform::KeyboardInput>>();
        world.init_resource::<Messages<regia_ui::UiEvent>>();
        let mut field = TextInput::default();
        // Multi-line, so a `\n` in `text` stays a line break.
        field.multiline = true;
        field.set_text(text);
        let e = world.spawn((field, NodeName("email".into()))).id();
        (world, e)
    }

    #[test]
    fn chords_parse_into_modifiers_and_keys() {
        let (mods, key) = parse_chord("Ctrl+Shift+Left").expect("chord");
        assert_eq!((mods, key.as_str()), (vec!["Control", "Shift"], "ArrowLeft"));
        let (mods, key) = parse_chord("Ctrl++").expect("plus");
        assert_eq!((mods, key.as_str()), (vec!["Control"], "+"));
        let (mods, key) = parse_chord("+").expect("bare plus");
        assert_eq!((mods.len(), key.as_str()), (0, "+"));
        assert_eq!(parse_chord("a").expect("letter").1, "a");
        let err = parse_chord("Hyper+a").expect_err("unknown modifier");
        assert_eq!(err.data.expect("data")["reason"], json!("ui_bad_key"));
    }

    #[test]
    fn key_writes_modifiers_around_the_key_in_order() {
        let (mut world, e) = world_with_field("abc");
        let out = key(&mut world, &json!({ "name": "email", "keys": ["Ctrl+A", "x"] })).expect("key");
        assert_eq!(out["node"], json!(e.to_bits()));
        assert_eq!(out["sent"], json!(2));
        assert_eq!(
            world.resource::<FocusState>().focused,
            Some(e),
            "naming a field focuses it"
        );
        let sent: Vec<(String, bool)> = world
            .resource::<Messages<regia_platform::KeyboardInput>>()
            .iter_current_update_messages()
            .map(|k| {
                (
                    k.logical_key.clone(),
                    k.state == regia_platform::ButtonState::Pressed,
                )
            })
            .collect();
        let want: Vec<(String, bool)> = [
            ("Control", true),
            ("A", true),
            ("A", false),
            ("Control", false),
            ("x", true),
            ("x", false),
        ]
        .iter()
        .map(|(k, d)| ((*k).to_owned(), *d))
        .collect();
        assert_eq!(sent, want);
    }

    /// ADR-7208, end to end through the dispatcher: a focused, laid-out field reports the rect it
    /// hands to the IME and its drawn caret in window px; typing moves both right.
    #[test]
    fn text_state_reports_the_ime_rect_and_visual_caret_through_the_dispatcher() {
        use regia_ui::prelude::*;
        let mut app = App::new();
        app.insert_resource(regia_ui::text::TextSystem::embedded_only());
        app.add_plugins(UiPlugin::code_only());
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        let root = app
            .world_mut()
            .spawn((
                UiRoot {
                    scale_mode: ScaleMode::ConstantPixelSize,
                    ..Default::default()
                },
                NodeStyle::new(Style {
                    size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                    padding: Edges::px(20.0),
                    ..Default::default()
                }),
            ))
            .id();
        let mut field = TextInput::default();
        field.set_text("ab");
        let e = app
            .world_mut()
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Points(200.0), Dimension::Points(36.0)],
                    padding: Edges::px(8.0),
                    ..Default::default()
                }),
                field,
                NodeName("email".into()),
            ))
            .id();
        app.world_mut().entity_mut(root).add_child(e);
        let call = |app: &mut App, method: &str, params: Value| {
            crate::dispatch::call_in_process(app.world_mut(), method, params)
                .expect("synchronous method")
                .expect("answered")
        };
        for _ in 0..3 {
            app.update();
        }
        let unfocused = call(&mut app, "ui.text_state", json!({ "name": "email" }));
        assert_eq!(unfocused["ime_rect"], Value::Null, "{unfocused}");
        // Naming the field focuses it.
        call(&mut app, "ui.key", json!({ "name": "email", "keys": ["End"] }));
        for _ in 0..3 {
            app.update();
        }
        let before = call(&mut app, "ui.text_state", json!({ "name": "email" }));
        let rect = before["ime_rect"]
            .as_array()
            .expect("an IME rect while focused")
            .clone();
        assert_eq!(rect.len(), 4);
        let x0 = rect[0].as_f64().expect("x");
        assert!(x0 > 20.0, "inside the field, right of the root padding: {before}");
        assert_eq!(
            before["visual_caret"]["x"], rect[0],
            "the IME rect starts at the caret"
        );
        assert_eq!(
            app.world()
                .resource::<regia_platform::ImeCursorArea>()
                .rect
                .map(|r| f64::from(r[0])),
            Some(x0),
            "the same rect is handed to the platform layer"
        );
        call(&mut app, "ui.key", json!({ "name": "email", "keys": ["c", "d"] }));
        for _ in 0..3 {
            app.update();
        }
        let after = call(&mut app, "ui.text_state", json!({ "name": "email" }));
        let x1 = after["ime_rect"][0].as_f64().expect("x");
        assert!(x1 > x0, "typing moved the IME rect right: {x0} -> {x1}");
    }

    #[test]
    fn select_and_state_use_character_indices() {
        let (mut world, e) = world_with_field("héllo\nwörld");
        let out = text_select(
            &mut world,
            &json!({ "node": e.to_bits(), "anchor": 6, "caret": 8 }),
        )
        .expect("select");
        assert_eq!(out["selection"], json!([6, 8]));
        assert_eq!(out["selected_text"], json!("wö"));
        assert_eq!(out["caret"], json!(8));
        assert_eq!((out["line"].clone(), out["column"].clone()), (json!(1), json!(2)));
        assert_eq!(out["length"], json!(11));
        assert_eq!(out["focused"], json!(false));
        // Not focused and never laid out: no IME rect, no drawn caret (ADR-7208).
        assert_eq!(out["ime_rect"], Value::Null);
        assert_eq!(out["visual_caret"], Value::Null);
        assert_eq!(out["preedit_cursor"], Value::Null);
    }

    #[test]
    fn text_breaks_reports_dictionary_word_offsets() {
        let mut world = World::new();
        // Thai: words and character offsets (not bytes).
        let out = text_breaks(&mut world, &json!({ "text": "ฉันกินข้าว" })).expect("breaks");
        assert_eq!(out["words"], json!(["ฉัน", "กิน", "ข้าว"]));
        assert_eq!(out["breaks"], json!([3, 6]), "character offsets");
        assert_eq!(out["locale"], Value::Null);
        // The locale tag is accepted and echoed; segmentation follows the text's script.
        let out = text_breaks(&mut world, &json!({ "text": "ฉันกิน", "locale": "th-TH" })).expect("breaks");
        assert_eq!(out["locale"], json!("th-TH"));
        assert_eq!(out["words"], json!(["ฉัน", "กิน"]));
        // Lao, Khmer, Myanmar segment too.
        let out = text_breaks(&mut world, &json!({ "text": "ຂ້າພະເຈົ້າກິນເຂົ້າ" })).expect("breaks");
        assert_eq!(out["words"], json!(["ຂ້າພະເຈົ້າ", "ກິນ", "ເຂົ້າ"]));
        let out = text_breaks(&mut world, &json!({ "text": "ខ្ញុំញ៉ាំបាយ" })).expect("breaks");
        assert_eq!(out["words"], json!(["ខ្ញុំ", "ញ៉ាំ", "បាយ"]));
        let out = text_breaks(&mut world, &json!({ "text": "ကျွန်တော်စားဆန်" })).expect("breaks");
        assert_eq!(out["words"], json!(["ကျွန်တော်", "စား", "ဆန်"]));
        // No SA script: the dictionary has nothing to say (normal UAX #14 rules apply).
        let out = text_breaks(&mut world, &json!({ "text": "hello world" })).expect("breaks");
        assert_eq!(out["words"], json!([] as [&str; 0]));
        assert_eq!(out["breaks"], json!([] as [usize; 0]));
        // corr-agent finding 23 (ADR-5895): text with an SA character lists every break the wrap
        // path marks, UAX #14 ones in its Latin part included (`regia_ui` hands the same list to
        // the wrapper), not only the dictionary ones.
        let out = text_breaks(&mut world, &json!({ "text": "hello ฉันกิน" })).expect("breaks");
        assert_eq!(out["breaks"], json!([6, 9]));
        assert_eq!(out["words"], json!(["hello ", "ฉัน", "กิน"]));
        // Errors name their reason.
        let reason = |r: Result<Value, RpcError>| r.expect_err("fails").data.expect("data")["reason"].clone();
        assert_eq!(
            reason(text_breaks(&mut world, &json!({}))),
            json!("ui_text_missing")
        );
        assert_eq!(
            reason(text_breaks(&mut world, &json!({ "text": 5 }))),
            json!("bad_text")
        );
        assert_eq!(
            reason(text_breaks(&mut world, &json!({ "text": "x", "locale": 7 }))),
            json!("bad_locale")
        );
        assert_eq!(
            reason(text_breaks(
                &mut world,
                &json!({ "text": "x".repeat(MAX_BREAK_CHARS + 1) })
            )),
            json!("text_too_long")
        );
    }

    #[test]
    fn errors_name_their_reason() {
        let (mut world, _) = world_with_field("abc");
        let reason = |r: Result<Value, RpcError>| r.expect_err("fails").data.expect("data")["reason"].clone();
        assert_eq!(reason(text_state(&mut world, &json!({}))), json!("ui_no_focus"));
        assert_eq!(
            reason(text_state(&mut world, &json!({ "name": "nope" }))),
            json!("ui_node_not_found")
        );
        let plain = world.spawn(NodeName("plain".into())).id();
        assert_eq!(
            reason(text_state(&mut world, &json!({ "node": plain.to_bits() }))),
            json!("ui_not_text_input")
        );
        assert_eq!(
            reason(key(&mut world, &json!({ "keys": [] }))),
            json!("ui_key_empty")
        );
        assert_eq!(
            reason(text_select(&mut world, &json!({ "name": "email" }))),
            json!("ui_bad_param")
        );
        let secret = {
            let mut f = TextInput::default();
            f.secret = Some('•');
            f.set_text("pw");
            world.spawn(f).id()
        };
        let out = text_state(&mut world, &json!({ "node": secret.to_bits() })).expect("state");
        assert_eq!(out["text"], json!("••"), "masked fields never reveal their text");
        assert_eq!(out["selected_text"], Value::Null);
    }
}
