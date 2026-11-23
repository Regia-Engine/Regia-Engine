// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): chord parsing, canonical text, the registry's resolution and
//! conflict rules. Pure data, no ECS app, no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_ui::interaction::UiModifiers;
use regia_ui::shortcuts::{
    Chord, ConflictKind, Resolution, ShortcutError, ShortcutScope, UiShortcut, UiShortcuts,
};

fn chord(text: &str) -> Chord {
    Chord::parse(text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
}

#[test]
fn parse_accepts_the_documented_spellings() {
    for (text, want) in [
        ("Ctrl+Shift+S", "Ctrl+Shift+S"),
        ("shift+ctrl+s", "Ctrl+Shift+S"),
        ("CONTROL+Option+Cmd+Delete", "Ctrl+Alt+Meta+Delete"),
        ("Super+Win+A", ""),
        ("Esc", "Escape"),
        ("return", "Enter"),
        ("PgUp", "PageUp"),
        ("Ctrl+Up", "Ctrl+ArrowUp"),
        ("f12", "F12"),
        ("F24", "F24"),
        ("Ctrl++", "Ctrl++"),
        ("+", "+"),
        ("  Alt+F4  ", "Alt+F4"),
        ("Ctrl + S", "Ctrl+S"),
        (" Ctrl+ Shift + 1", "Ctrl+Shift+1"),
        ("Space", "Space"),
        ("Ctrl+Spacebar", "Ctrl+Space"),
        ("é", "É"),
    ] {
        if want.is_empty() {
            // Two names for the same modifier in one chord are a repeat.
            assert!(Chord::parse(text).is_err(), "{text:?}");
            continue;
        }
        assert_eq!(chord(text).canonical(), want, "{text:?}");
    }
}

#[test]
fn parse_rejects_with_the_bad_chord_code() {
    for text in [
        "",
        " ",
        "Ctrl+",
        "Ctrl+Shift",
        "Ctrl+Ctrl+S",
        "Ctrl+A+B",
        "Hyper+S",
        "F0",
        "F25",
        "Fn",
        "Ctrl+Nope",
        "+++",
        "Ctrl+Shift+Alt+Meta",
        "Shift",
        "Alt",
        "Ctrl+Meta",
        "\u{7}",
        "Ctrl+\n",
    ] {
        let err = Chord::parse(text).expect_err(text);
        assert_eq!(err.code(), "bad_chord", "{text:?}");
        assert!(err.to_string().contains("bad chord"), "{err}");
    }
}

#[test]
fn canonical_text_parses_back_to_the_same_chord_for_every_key_kind() {
    let keys = [
        "A",
        "z",
        "5",
        "-",
        "=",
        "[",
        ";",
        "É",
        "ß",
        "ǆ",
        "日",
        "Space",
        "Tab",
        "Escape",
        "Enter",
        "Backspace",
        "Delete",
        "Insert",
        "Home",
        "End",
        "PageUp",
        "PageDown",
        "ArrowUp",
        "ArrowDown",
        "ArrowLeft",
        "ArrowRight",
        "F1",
        "F9",
        "F24",
        "+",
    ];
    for key in keys {
        for mods in 0..16u8 {
            let m = UiModifiers {
                ctrl: mods & 1 != 0,
                alt: mods & 2 != 0,
                shift: mods & 4 != 0,
                meta: mods & 8 != 0,
            };
            let original = Chord::from_event(m, key).unwrap_or_else(|| panic!("{key:?}"));
            let text = original.canonical();
            let back = Chord::parse(&text).unwrap_or_else(|e| panic!("{text:?} (from {key:?}): {e}"));
            assert_eq!(back, original, "{text:?}");
            // Display never panics and always ends with the key.
            assert!(original.display(false).ends_with(&original.key));
            assert!(original.display(true).ends_with(&original.key));
        }
    }
}

#[test]
fn modifier_keys_and_nameless_keys_are_not_chords() {
    let none = UiModifiers::default();
    for key in [
        "Shift",
        "Control",
        "ctrl",
        "Alt",
        "AltGraph",
        "Meta",
        "Super",
        "OS",
        "Command",
        "",
        "\u{1b}",
        "Dead",
        "Unidentified",
    ] {
        assert_eq!(Chord::from_event(none, key), None, "{key:?}");
    }
    assert_eq!(Chord::from_event(none, " ").unwrap().key, "Space");
    assert_eq!(
        Chord::from_event(none, "a").unwrap(),
        Chord::from_event(none, "A").unwrap()
    );
}

#[test]
fn display_styles() {
    let c = chord("Ctrl+Alt+Shift+Meta+K");
    assert_eq!(c.display(false), "Ctrl+Alt+Shift+Super+K");
    assert_eq!(c.display(true), "⌃⌥⇧⌘K");
    assert_eq!(chord("Escape").display(false), "Escape");
    assert_eq!(chord("Escape").display(true), "Escape");
}

#[test]
fn scope_names_round_trip() {
    for (text, want) in [
        ("", ShortcutScope::Global),
        ("  ", ShortcutScope::Global),
        ("global", ShortcutScope::Global),
        ("GLOBAL", ShortcutScope::Global),
        (" inventory ", ShortcutScope::Context("inventory".to_owned())),
        ("Inventory", ShortcutScope::Context("Inventory".to_owned())),
    ] {
        let got = ShortcutScope::parse(text);
        assert_eq!(got, want, "{text:?}");
        assert_eq!(ShortcutScope::parse(got.name()), got);
    }
}

fn reg(shortcuts: &mut UiShortcuts, key: &str, action: &str, scope: ShortcutScope) {
    shortcuts
        .register(UiShortcut::new(chord(key), action).in_scope(scope))
        .unwrap_or_else(|e| panic!("{key} {action}: {e}"));
}

fn ctx(name: &str) -> ShortcutScope {
    ShortcutScope::Context(name.to_owned())
}

#[test]
fn registering_the_same_binding_twice_is_the_only_error() {
    let mut s = UiShortcuts::default();
    reg(&mut s, "Ctrl+S", "save", ShortcutScope::Global);
    let err = s
        .register(UiShortcut::new(chord("ctrl+s"), "save"))
        .expect_err("same chord, scope and action");
    assert_eq!(err.code(), "duplicate_shortcut");
    assert!(matches!(err, ShortcutError::Duplicate(_)));
    // Different action, scope or chord is fine.
    reg(&mut s, "Ctrl+S", "save-all", ShortcutScope::Global);
    reg(&mut s, "Ctrl+S", "save", ctx("editor"));
    reg(&mut s, "Ctrl+Shift+S", "save", ShortcutScope::Global);
    assert_eq!(s.shortcuts().len(), 4);
    assert_eq!(s.unregister("save"), 3);
    assert_eq!(s.unregister("save"), 0);
    assert_eq!(s.shortcuts().len(), 1);
}

#[test]
fn resolution_prefers_active_context_over_global_and_reports_conflicts() {
    let mut s = UiShortcuts::default();
    reg(&mut s, "Escape", "pause", ShortcutScope::Global);
    reg(&mut s, "Escape", "close-menu", ctx("menu"));
    reg(&mut s, "Escape", "close-map", ctx("map"));
    let esc = chord("Escape");
    let action = |s: &UiShortcuts| match s.resolve(&esc) {
        Resolution::One(hit) => hit.action.clone(),
        Resolution::None => "<none>".to_owned(),
        Resolution::Conflict(hits) => {
            let mut names: Vec<_> = hits.iter().map(|h| h.action.clone()).collect();
            names.sort();
            format!("<conflict {}>", names.join(","))
        }
    };
    assert_eq!(action(&s), "pause", "no context active");
    s.set_contexts(["menu"]);
    assert_eq!(
        action(&s),
        "close-menu",
        "an active context shadows the global one"
    );
    s.set_contexts(["menu", "map"]);
    assert_eq!(
        action(&s),
        "<conflict close-map,close-menu>",
        "two active contexts on one chord"
    );
    s.set_contexts(["unknown"]);
    assert_eq!(action(&s), "pause");
    s.set_contexts(Vec::<String>::new());
    assert!(s.contexts().is_empty());
    assert_eq!(s.resolve(&chord("Ctrl+Z")), Resolution::None);
}

#[test]
fn conflicts_are_classified_and_ignore_the_active_contexts() {
    let mut s = UiShortcuts::default();
    reg(&mut s, "F5", "reload", ShortcutScope::Global);
    reg(&mut s, "F5", "refresh", ShortcutScope::Global);
    reg(&mut s, "F5", "reload-ui", ctx("ui"));
    reg(&mut s, "F5", "reload-map", ctx("map"));
    reg(&mut s, "F6", "other", ShortcutScope::Global);
    let kinds: Vec<_> = s
        .conflicts()
        .iter()
        .map(|c| (c.actions.clone(), c.kind))
        .collect();
    let name = |a: &str, b: &str| [a.to_owned(), b.to_owned()];
    assert_eq!(
        kinds,
        [
            (name("reload", "refresh"), ConflictKind::SameScope),
            (name("reload", "reload-ui"), ConflictKind::Shadow),
            (name("reload", "reload-map"), ConflictKind::Shadow),
            (name("refresh", "reload-ui"), ConflictKind::Shadow),
            (name("refresh", "reload-map"), ConflictKind::Shadow),
            (name("reload-ui", "reload-map"), ConflictKind::Overlap),
        ]
    );
    for kind in [
        ConflictKind::SameScope,
        ConflictKind::Shadow,
        ConflictKind::Overlap,
    ] {
        assert!(kind.name().bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
    }
    // One action bound twice on the same chord (two scopes) is not a conflict with itself.
    let mut t = UiShortcuts::default();
    reg(&mut t, "F1", "help", ShortcutScope::Global);
    reg(&mut t, "F1", "help", ctx("ui"));
    assert!(t.conflicts().is_empty());
}

#[test]
fn activation_reports_why_an_action_cannot_fire() {
    let mut s = UiShortcuts::default();
    reg(&mut s, "Ctrl+B", "build", ctx("editor"));
    reg(&mut s, "Ctrl+P", "print", ShortcutScope::Global);
    reg(&mut s, "Ctrl+P", "plot", ShortcutScope::Global);
    assert_eq!(s.activation("nope").unwrap_err().code(), "unknown_action");
    assert_eq!(
        s.activation("build").unwrap_err().code(),
        "shortcut_inactive",
        "context not active"
    );
    assert_eq!(s.activation("print").unwrap_err().code(), "shortcut_ambiguous");
    s.set_contexts(["editor"]);
    let fired = s.activation("build").expect("active");
    assert_eq!((fired.action.as_str(), fired.scope), ("build", ctx("editor")));
    // A context shortcut on a global's chord hides the global: inactive, not unknown.
    let mut t = UiShortcuts::default();
    reg(&mut t, "Escape", "pause", ShortcutScope::Global);
    reg(&mut t, "Escape", "back", ctx("menu"));
    t.set_contexts(["menu"]);
    assert_eq!(t.activation("pause").unwrap_err().code(), "shortcut_inactive");
    assert!(t.activation("back").is_ok());
}

#[test]
fn chord_labels_prefer_live_then_global_then_first() {
    // Labels use the build platform's style (`⌃⇧S` on macOS, `Ctrl+Shift+S` elsewhere).
    let label = |key: &str| chord(key).platform_display();
    let mut s = UiShortcuts::default();
    assert_eq!(s.chord_label("save"), None);
    reg(&mut s, "Ctrl+Shift+S", "save", ctx("editor"));
    reg(&mut s, "Ctrl+S", "save", ctx("other"));
    assert_eq!(
        s.chord_label("save").as_deref(),
        Some(label("Ctrl+Shift+S").as_str()),
        "first when nothing is live"
    );
    s.set_contexts(["other"]);
    assert_eq!(
        s.chord_label("save").as_deref(),
        Some(label("Ctrl+S").as_str()),
        "the live one"
    );
    reg(&mut s, "F2", "save", ShortcutScope::Global);
    s.set_contexts(Vec::<String>::new());
    assert_eq!(
        s.chord_label("save").as_deref(),
        Some("F2"),
        "a global one when no context is live"
    );
    assert_eq!(s.chord_label("missing"), None);
}
