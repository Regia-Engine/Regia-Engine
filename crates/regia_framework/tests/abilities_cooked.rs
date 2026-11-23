// SPDX-License-Identifier: MPL-2.0
//! Spec 53 § 4.1 cooked form (`abilities-cooked`, ADR-7216): a little-endian `u32` version, then
//! postcard of the asset. Cooked bytes load to the same values as the RON they were cooked from,
//! through the same validation; damaged bytes are errors with a code, never a panic.

use regia_framework::abilities::*;

const ATTRS: &str = r#"(format_version: 1, description: "Hero stats", attributes: [
    (name: "health_max", base: 100.0, min: 1.0, max: 9999.0),
    (name: "health", base: 100.0, min: 0.0, max: 9999.0, max_from: Some("health_max")),
    (name: "strength", base: 10.0, min: 0.0, max: 100.0),
    (name: "armor", base: 0.0, min: 0.0, max: 1000.0, derived: Some((inputs: [("strength", 2.0)], offset: 0.5))),
])"#;

const POISON: &str = r#"(format_version: 1, name: "poison",
 lifetime: Timed(Const(6.0)), pulse: Some((every: 1.0, on_apply: false)),
 base_edits: [(attribute: "health", op: AddPre, amount: Curve((table: "", name: "poison")))],
 live_mods: [(attribute: "move_speed", op: Scale, amount: Const(0.8))],
 status_tags: ["Status.Poisoned"], kind_tags: ["Effect.Damage.Poison"],
 stack: Some((key: ByTarget, cap: 3, on_add: (time: Reset, pulse: Keep), when_full: RejectRenew,
              on_lapse: EndAll, cue_each_stack: false)),
 cues: [(tag: "Cue.Status.Poison", at: Sustained)],
 curves: [(name: "poison", points: [(1.0, -2.0), (10.0, -9.0)])])"#;

const MANA: &str = r#"(format_version: 1, name: "mana_10", lifetime: Instant,
 base_edits: [(attribute: "mana", op: AddPre, amount: Const(-10.0))])"#;

const COOLDOWN: &str = r#"(format_version: 1, name: "cd", lifetime: Timed(Const(2.0)),
 status_tags: ["Cooldown.Fire"])"#;

const BOLT: &str = r#"(format_version: 1, name: "fire_bolt", kind_tags: ["Ability.Spell.Fire"], run_tags: ["State.Casting"],
 blocks: Some(Has("Ability.Spell")), owner_forbids: Some(AnyOf([Has("Status.Silenced"), HasExact("Status.Stunned")])),
 cost: Some("effects/mana_10.effect.ron"), cooldown: Some("effects/cd.effect.ron"),
 charge: OnStart, run_state: ScratchOnly, when_running: Reject, triggers: [], max_run_ticks: Some(120),
 input_code: Some(1), run_site: HostOnly, cues: [(tag: "Cue.Cast.Fire", at: Applied)])"#;

/// A registry with the RON loaded, and one with the cooked bytes of the same files loaded.
fn both() -> (AbilityAssets, AbilityAssets) {
    let files = [
        ("stats/hero.attrs.ron", ATTRS),
        ("effects/poison.effect.ron", POISON),
        ("effects/mana_10.effect.ron", MANA),
        ("effects/cd.effect.ron", COOLDOWN),
        ("abilities/bolt.ability.ron", BOLT),
    ];
    let mut text = AbilityAssets::default();
    let mut cooked = AbilityAssets::default();
    for (path, ron) in files {
        let d = text.load_text(path, ron, None, None);
        assert!(!has_errors(&d), "{path}: {d:?}");
        let bytes = text
            .cook_text(path, ron, None, None)
            .unwrap_or_else(|d| panic!("{path}: {d:?}"));
        assert_eq!(&bytes[..4], &ABILITIES_COOKED_VERSION.to_le_bytes());
        let d = cooked.load_cooked(path, &bytes, None, None);
        assert!(!has_errors(&d), "{path}: {d:?}");
    }
    (text, cooked)
}

#[test]
fn cooked_bytes_load_to_the_same_values_as_the_ron() {
    let (text, cooked) = both();
    assert_eq!(
        text.attributes("stats/hero.attrs.ron"),
        cooked.attributes("stats/hero.attrs.ron")
    );
    assert!(cooked.attributes("stats/hero.attrs.ron").is_some());
    for path in [
        "effects/poison.effect.ron",
        "effects/mana_10.effect.ron",
        "effects/cd.effect.ron",
    ] {
        assert!(cooked.effect(path).is_some(), "{path}");
        assert_eq!(text.effect(path), cooked.effect(path), "{path}");
    }
    assert!(cooked.ability("abilities/bolt.ability.ron").is_some());
    assert_eq!(
        text.ability("abilities/bolt.ability.ron"),
        cooked.ability("abilities/bolt.ability.ron")
    );
    // The same asset cooks to the same bytes (a pak entry is stable).
    let a = text.cook_text("effects/poison.effect.ron", POISON, None, None);
    let b = text.cook_text("effects/poison.effect.ron", POISON, None, None);
    assert_eq!(a, b);
}

#[test]
fn a_cook_fails_on_what_the_text_load_refuses() {
    let assets = AbilityAssets::default();
    // The ability's cost and cooldown are not loaded: `reference_missing`, as `load_text` says.
    let d = assets
        .cook_text("abilities/bolt.ability.ron", BOLT, None, None)
        .expect_err("refused");
    assert!(d.iter().any(|d| d.code == "reference_missing"), "{d:?}");
    let d = assets
        .cook_text(
            "effects/x.effect.ron",
            "(format_version: 9, name: \"x\", lifetime: Instant)",
            None,
            None,
        )
        .expect_err("refused");
    assert_eq!(d[0].code, "format_too_new");
    let d = assets
        .cook_text("effects/x.txt", POISON, None, None)
        .expect_err("refused");
    assert_eq!(d[0].code, "parse_error");
}

#[test]
fn damaged_cooked_bytes_are_errors_and_never_panic() {
    let assets = AbilityAssets::default();
    let bytes = assets
        .cook_text("stats/hero.attrs.ron", ATTRS, None, None)
        .expect("cook");
    for len in 0..bytes.len() {
        let mut fresh = AbilityAssets::default();
        let d = fresh.load_cooked("stats/hero.attrs.ron", &bytes[..len], None, None);
        assert!(has_errors(&d), "truncated to {len}");
        assert!(fresh.attributes("stats/hero.attrs.ron").is_none());
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    let mut fresh = AbilityAssets::default();
    let d = fresh.load_cooked("stats/hero.attrs.ron", &trailing, None, None);
    assert_eq!(d[0].code, "cooked_bad_file", "{d:?}");
    for version in [0u32, ABILITIES_COOKED_VERSION + 1] {
        let mut other = bytes.clone();
        other[..4].copy_from_slice(&version.to_le_bytes());
        let d = fresh.load_cooked("stats/hero.attrs.ron", &other, None, None);
        assert_eq!(d[0].code, "cooked_unsupported_version", "{version}: {d:?}");
    }
    // A cooked effect under an attribute path is refused, not loaded as something else.
    let effect = AbilityAssets::default()
        .cook_text("effects/cd.effect.ron", COOLDOWN, None, None)
        .expect("cook");
    let d = fresh.load_cooked("stats/hero.attrs.ron", &effect, None, None);
    assert_eq!(d[0].code, "cooked_kind_mismatch", "{d:?}");
    assert!(fresh.attributes("stats/hero.attrs.ron").is_none());
}

#[test]
fn cooked_validation_is_the_text_validation() {
    // A cooked effect whose bytes were hand-built with a bad value is refused by the rule's own code.
    let mut bad: EffectAsset = parse_ron(COOLDOWN, EFFECT_FORMAT_VERSION).expect("parse");
    bad.lifetime = Lifetime::Timed(Magnitude::Const(f32::NAN));
    let bytes = cooked_bytes(&CookedAbilityAsset::Effect(bad.clone())).expect("encode");
    let mut cooked = AbilityAssets::default();
    let from_cooked = cooked.load_cooked("effects/cd.effect.ron", &bytes, None, None);
    let from_text = validate_effect(&bad, None);
    assert!(has_errors(&from_text));
    let codes = |d: &[Diagnostic]| d.iter().map(|d| d.code).collect::<Vec<_>>();
    assert_eq!(codes(&from_cooked), codes(&from_text));
    assert!(cooked.effect("effects/cd.effect.ron").is_none());
}
