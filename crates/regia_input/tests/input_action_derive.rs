// SPDX-License-Identifier: MPL-2.0
//! Spec 11 § 7 `#[derive(InputAction)]` (lane post05-ui-input, ADR-7070): a fieldless enum names its
//! actions (the variant name, or `#[action(name = "...")]`), declares their kinds, and the typed calls on
//! `PlayerActions` read the same state as the string API.

use regia_input::InputAction;
use regia_input::prelude::*;

#[derive(InputAction, Debug, Clone, Copy, PartialEq, Eq)]
enum Gameplay {
    Jump,
    #[action(kind = Axis2D)]
    Move,
    #[action(name = "fire_primary")]
    Fire,
    #[action(name = "look", kind = Axis2D)]
    Look,
}

#[test]
fn the_derive_generates_the_expected_action_names_and_kinds() {
    let names: Vec<&str> = Gameplay::ALL.iter().map(|a| a.name()).collect();
    assert_eq!(names, ["Jump", "Move", "fire_primary", "look"]);
    let kinds: Vec<ActionKind> = Gameplay::ALL.iter().map(|a| a.kind()).collect();
    assert_eq!(
        kinds,
        [
            ActionKind::Button,
            ActionKind::Axis2D,
            ActionKind::Button,
            ActionKind::Axis2D
        ]
    );
    assert_eq!(Gameplay::from_name("fire_primary"), Some(Gameplay::Fire));
    assert_eq!(
        Gameplay::from_name("Fire"),
        None,
        "the renamed variant answers to its new name only"
    );

    let config = InputConfig::new().declare::<Gameplay>();
    for action in Gameplay::ALL {
        assert_eq!(config.kind(action.name()), Some(action.kind()));
    }
}

#[test]
fn typed_reads_match_the_string_api() {
    let actions = Actions::default();
    assert!(!actions.pressed_action(Gameplay::Jump));
    assert_eq!(actions.axis2d_action(Gameplay::Move), actions.axis2d("Move"));
}
