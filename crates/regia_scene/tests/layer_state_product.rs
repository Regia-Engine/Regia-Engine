// SPDX-License-Identifier: MPL-2.0
//! Bounded product coverage for the requested/effective layer contract in spec 37 § 4.
//! The reference calculations below walk the declared parent map and do not call production
//! `effective` to derive expected values.

use regia_scene::layers::{LayerMembership, LayerPeers, LayerState, LayerTable, Side};

const NAMES: [&str; 5] = ["root", "left", "left_leaf", "right", "right_leaf"];
const PARENTS: [Option<usize>; 5] = [None, Some(0), Some(1), Some(0), Some(3)];
const STATES: [LayerState; 3] = [LayerState::Absent, LayerState::Parked, LayerState::Live];

fn state_at(mut code: usize, index: usize) -> LayerState {
    for _ in 0..index {
        code /= STATES.len();
    }
    STATES[code % STATES.len()]
}

fn reference_effective(requested: &[LayerState; 5], index: usize) -> LayerState {
    let mut result = requested[index];
    let mut parent = PARENTS[index];
    while let Some(ancestor) = parent {
        result = result.min(requested[ancestor]);
        parent = PARENTS[ancestor];
    }
    result
}

fn membership(mask: usize) -> LayerMembership {
    LayerMembership {
        layers: NAMES
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .map(|(_, name)| (*name).to_owned())
            .collect(),
    }
}

fn reference_presence(effective: &[LayerState; 5], mask: usize) -> LayerState {
    NAMES
        .iter()
        .enumerate()
        .filter(|(index, _)| mask & (1 << index) != 0)
        .map(|(index, _)| effective[index])
        .min()
        .unwrap_or(LayerState::Live)
}

fn table(requested: [LayerState; 5], reverse: bool) -> LayerTable {
    let mut table = LayerTable::default();
    let indices: Vec<usize> = if reverse {
        (0..NAMES.len()).rev().collect()
    } else {
        (0..NAMES.len()).collect()
    };
    for index in indices {
        table.insert_layer(
            NAMES[index],
            PARENTS[index].map(|parent| NAMES[parent]),
            requested[index],
            LayerPeers::Everyone,
        );
    }
    table
}

#[test]
fn all_requested_state_products_match_ancestor_and_membership_oracles() {
    let combinations = STATES.len().pow(NAMES.len() as u32);
    for code in 0..combinations {
        let requested = std::array::from_fn(|index| state_at(code, index));
        for reverse in [false, true] {
            let table = table(requested, reverse);
            let effective: [LayerState; 5] =
                std::array::from_fn(|index| reference_effective(&requested, index));
            for index in 0..NAMES.len() {
                assert_eq!(
                    table.requested(NAMES[index]),
                    Some(requested[index]),
                    "code={code} reverse={reverse} layer={}",
                    NAMES[index]
                );
                assert_eq!(
                    table.effective(NAMES[index]),
                    Some(effective[index]),
                    "code={code} reverse={reverse} layer={}",
                    NAMES[index]
                );
            }
            for mask in 1..(1 << NAMES.len()) {
                assert_eq!(
                    table.presence(&membership(mask)),
                    reference_presence(&effective, mask),
                    "code={code} reverse={reverse} membership={mask:#07b}"
                );
            }
        }
    }
}

#[test]
fn parent_clamp_retains_child_request_until_parent_opens() {
    let mut table = table([LayerState::Live; 5], false);
    assert!(
        table
            .request("left", LayerState::Absent, false, Side::Local)
            .expect("known layer")
    );
    assert_eq!(table.requested("left_leaf"), Some(LayerState::Live));
    assert_eq!(table.effective("left_leaf"), Some(LayerState::Absent));
    assert_eq!(
        table.presence(&membership((1 << 1) | (1 << 2))),
        LayerState::Absent
    );

    assert!(
        table
            .request("left", LayerState::Parked, false, Side::Local)
            .expect("known layer")
    );
    assert_eq!(table.requested("left_leaf"), Some(LayerState::Live));
    assert_eq!(table.effective("left_leaf"), Some(LayerState::Parked));
    assert!(
        table
            .request("left", LayerState::Live, false, Side::Local)
            .expect("known layer")
    );
    assert_eq!(table.requested("left_leaf"), Some(LayerState::Live));
    assert_eq!(table.effective("left_leaf"), Some(LayerState::Live));
}

#[test]
fn recursive_and_nonrecursive_sequences_follow_the_reference_requests() {
    let mut table = table([LayerState::Live; 5], false);
    let mut requested = [LayerState::Live; 5];
    for (layer, state, recursive) in [
        ("root", LayerState::Absent, false),
        ("left", LayerState::Parked, true),
        ("root", LayerState::Live, false),
        ("right", LayerState::Absent, false),
        ("root", LayerState::Parked, true),
        ("root", LayerState::Live, false),
    ] {
        table
            .request(layer, state, recursive, Side::Local)
            .expect("known sequence layer");
        let index = NAMES
            .iter()
            .position(|name| *name == layer)
            .expect("sequence layer is in the reference model");
        requested[index] = state;
        if recursive {
            for child in 0..NAMES.len() {
                let mut parent = PARENTS[child];
                while let Some(ancestor) = parent {
                    if ancestor == index {
                        requested[child] = state;
                        break;
                    }
                    parent = PARENTS[ancestor];
                }
            }
        }
        for index in 0..NAMES.len() {
            assert_eq!(table.requested(NAMES[index]), Some(requested[index]));
            assert_eq!(
                table.effective(NAMES[index]),
                Some(reference_effective(&requested, index))
            );
        }
    }
}

#[test]
fn recursive_requests_skip_unowned_descendants_and_unowned_targets_are_noops() {
    let mut table = LayerTable::default();
    table.insert_layer("root", None, LayerState::Live, LayerPeers::Everyone);
    table.insert_layer("server", Some("root"), LayerState::Live, LayerPeers::ServerOnly);
    table.insert_layer("client", Some("root"), LayerState::Live, LayerPeers::ClientsOnly);
    table.insert_layer(
        "client_leaf",
        Some("client"),
        LayerState::Live,
        LayerPeers::ClientsOnly,
    );

    assert!(
        !table
            .request("server", LayerState::Absent, false, Side::Client)
            .expect("known layer")
    );
    assert_eq!(table.requested("server"), Some(LayerState::Live));
    assert!(
        table
            .request("root", LayerState::Parked, true, Side::Server)
            .expect("known layer")
    );
    assert_eq!(table.requested("root"), Some(LayerState::Parked));
    assert_eq!(table.requested("server"), Some(LayerState::Parked));
    assert_eq!(table.requested("client"), Some(LayerState::Live));
    assert_eq!(table.requested("client_leaf"), Some(LayerState::Live));

    assert!(
        table
            .request("server", LayerState::Absent, false, Side::Server)
            .expect("known layer")
    );
    assert_eq!(table.effective("server"), Some(LayerState::Absent));
    assert_eq!(table.effective("client_leaf"), Some(LayerState::Parked));
}
