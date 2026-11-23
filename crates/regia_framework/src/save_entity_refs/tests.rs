// SPDX-License-Identifier: MPL-2.0
//! Unit tests of the type scan, the reflection walk, the markers and the format 1 layout.

use std::collections::HashMap;

use super::*;

#[derive(Reflect, Clone, Debug, PartialEq)]
struct Inner {
    rider: Option<Entity>,
    driver: Entity,
}

#[derive(Reflect, Clone, Debug, PartialEq)]
struct Holder {
    plain: Entity,
    maybe: Option<Entity>,
    list: Vec<Entity>,
    pair: [Entity; 2],
    inner: Inner,
    by_slot: HashMap<u32, Entity>,
    speed: f32,
}

#[derive(Reflect, Clone, Debug, Default)]
struct NoRefs {
    speed: f32,
    name: String,
}

#[derive(Reflect, Clone, Debug, Default)]
struct KeyedByEntity {
    weights: HashMap<Entity, f32>,
}

fn registry() -> TypeRegistry {
    let mut registry = TypeRegistry::default();
    registry.register::<Holder>();
    registry.register::<NoRefs>();
    registry.register::<KeyedByEntity>();
    registry
}

fn e(index: u32) -> Entity {
    Entity::from_raw_u32(index).expect("small index")
}

#[test]
fn scan_finds_entities_through_every_container() {
    let registry = registry();
    let mut scans = Scans::default();
    assert_eq!(
        scans.get(&registry, TypeId::of::<Holder>()),
        Scan {
            refs: true,
            unsupported: false
        }
    );
    assert!(!scans.get(&registry, TypeId::of::<NoRefs>()).any());
    let keyed = scans.get(&registry, TypeId::of::<KeyedByEntity>());
    assert!(
        keyed.unsupported,
        "an Entity map key cannot be rewritten in place"
    );
}

#[test]
fn walk_maps_every_field_and_clears_the_innermost_option() {
    let mut holder = Holder {
        plain: e(1),
        maybe: Some(e(2)),
        list: vec![e(1), Entity::PLACEHOLDER],
        pair: [e(1), e(3)],
        inner: Inner {
            rider: Some(e(9)),
            driver: e(1),
        },
        by_slot: HashMap::from([(7, e(3))]),
        speed: 2.0,
    };
    // 9 has no target; every other entity moves to index + 100.
    let mut seen = Vec::new();
    let cleared = walk(&mut holder, &mut |target, field| {
        seen.push(field.to_string());
        if target.index_u32() == 9 {
            Leaf::Missing
        } else {
            Leaf::To(e(target.index_u32() + 100))
        }
    })
    .expect("only an optional reference is missing");
    assert_eq!(cleared, vec!["inner.rider".to_string()]);
    assert_eq!(holder.plain, e(101));
    assert_eq!(holder.maybe, Some(e(102)));
    assert_eq!(
        holder.list,
        vec![e(101), Entity::PLACEHOLDER],
        "the placeholder stays"
    );
    assert_eq!(holder.pair, [e(101), e(103)]);
    assert_eq!(holder.inner.rider, None);
    assert_eq!(holder.inner.driver, e(101));
    assert_eq!(holder.by_slot.get(&7), Some(&e(103)));
    assert_eq!(holder.speed, 2.0);
    assert!(
        seen.contains(&"pair[1]".to_string()) && seen.contains(&"by_slot[..]".to_string()),
        "{seen:?}"
    );
}

#[test]
fn walk_reports_a_plain_reference_without_a_target() {
    let mut inner = Inner {
        rider: None,
        driver: e(4),
    };
    let err = walk(&mut inner, &mut |_, _| Leaf::Missing).expect_err("driver has no target");
    assert_eq!(err, "driver");
}

#[test]
fn markers_round_trip_and_never_collide_with_the_placeholder() {
    for id in [0usize, 1, 77, 1 << 20] {
        let m = marker(id).expect("encodable");
        assert_eq!(marker_id(m), Some(id));
        assert_ne!(m, Entity::PLACEHOLDER);
    }
    assert_eq!(marker((u32::MAX - 1) as usize), None);
    assert_eq!(marker(u32::MAX as usize + 1), None);
}

fn sample_raw(version: u32) -> SaveFileRaw {
    let mut components = BTreeMap::new();
    components.insert("Health".to_string(), vec![1, 2, 3]);
    SaveFileRaw {
        version,
        game_version: "1.0".into(),
        engine_version: "test".into(),
        timestamp_secs: 5.0,
        thumbnail: None,
        entities: vec![SavedEntity { components }],
        resources: vec![SavedResource {
            type_path: "game::Score".into(),
            data: vec![4],
        }],
        entity_ids: vec![SaveEntityId::Runtime(0)],
    }
}

#[test]
fn format_1_layout_has_no_table_and_reads_back() {
    let v1 = encode_payload(&sample_raw(1)).expect("encode v1");
    let v2 = encode_payload(&sample_raw(2)).expect("encode v2");
    assert!(v2.len() > v1.len(), "format 2 appends the table");
    let back = decode_payload(1, &v1).expect("decode v1");
    assert!(back.entity_ids.is_empty());
    assert_eq!(back.entities[0].components["Health"], vec![1, 2, 3]);
    let back = decode_payload(2, &v2).expect("decode v2");
    assert_eq!(back.entity_ids, vec![SaveEntityId::Runtime(0)]);
    assert!(decode_payload(2, &v1).is_err(), "format 1 bytes lack the table");
    // Both layouts share the header prefix.
    for payload in [&v1, &v2] {
        let (game, engine, ts, thumb) = decode_header_fields(payload).expect("header");
        assert_eq!(
            (game.as_str(), engine.as_str(), ts, thumb),
            ("1.0", "test", 5.0, false)
        );
    }
}

#[test]
fn upgrade_rewrites_format_1_bytes_with_an_empty_table() {
    let payload = encode_payload(&sample_raw(1)).expect("encode v1");
    let mut bytes = SAVE_MAGIC.to_vec();
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&crate::save_game::crc32(&payload).to_le_bytes());
    let upgraded = upgrade_save_bytes(&bytes).expect("upgrade");
    assert_eq!(&upgraded[4..8], &2u32.to_le_bytes());
    let raw = decode_payload(2, &upgraded[8..upgraded.len() - 4]).expect("format 2 payload");
    assert_eq!(raw.version, 2);
    assert!(
        raw.entity_ids.is_empty(),
        "references are converted by the game's next load"
    );
    assert_eq!(
        upgrade_save_bytes(&upgraded).expect("current"),
        upgraded,
        "current bytes stay"
    );
    let mut damaged = bytes.clone();
    damaged[10] ^= 0xff;
    assert!(matches!(upgrade_save_bytes(&damaged), Err(SaveError::Corrupt(_))));
    assert!(matches!(upgrade_save_bytes(b"nope"), Err(SaveError::Corrupt(_))));
}

/// A payload older than format 1 needs a step the game registers: the tool cannot run it, so it
/// refuses and leaves the file for the game's own load instead of stamping it current.
#[test]
fn upgrade_refuses_a_step_only_the_game_can_migrate() {
    let payload = encode_payload(&sample_raw(0)).expect("encode v0");
    let mut bytes = SAVE_MAGIC.to_vec();
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&crate::save_game::crc32(&payload).to_le_bytes());
    match upgrade_save_bytes(&bytes) {
        Err(SaveError::Deserialize(msg)) => assert!(msg.contains("game's migration from version 0"), "{msg}"),
        other => panic!("{other:?}"),
    }
}

/// A payload whose first bytes are not a decodable version varint (a crafted file: every real
/// writer encodes version 0..=2 as one byte under 0x80) must be refused by name, never loaded by
/// falling back to the header word, which the CRC does not cover.
#[test]
fn upgrade_refuses_a_payload_without_a_version_varint() {
    for header in [0u32, 1] {
        for payload in [&[0x80, 0x80, 0x80, 0x80, 0x80][..], &[][..]] {
            let mut bytes = SAVE_MAGIC.to_vec();
            bytes.extend_from_slice(&header.to_le_bytes());
            bytes.extend_from_slice(payload);
            bytes.extend_from_slice(&crate::save_game::crc32(payload).to_le_bytes());
            match upgrade_save_bytes(&bytes) {
                Err(SaveError::Deserialize(msg)) => assert!(
                    msg.contains("decodable version"),
                    "header {header}, payload {payload:?}: {msg}"
                ),
                other => panic!("header {header}, payload {payload:?}: {other:?}"),
            }
        }
    }
}

#[test]
fn keys_name_the_kind() {
    assert_eq!(SaveEntityId::Runtime(3).key(), "runtime:3");
    assert_eq!(
        SaveEntityId::Level { guid: 0xab, level: 1 }.key(),
        format!("level:{:032x}", 0xab)
    );
    assert_eq!(SaveEntityId::Lost.key(), "lost");
}
