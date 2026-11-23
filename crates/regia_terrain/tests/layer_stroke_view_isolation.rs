// SPDX-License-Identifier: MPL-2.0
//! Regression coverage for layer-major `LayerWeightLayer` stroke views.

use glam::IVec2;
use regia_terrain::LayerWeightMap;
use regia_terrain::brush::{StrokeSource, StrokeTarget};

const WIDTH: usize = 5;
const HEIGHT: usize = 3;
const LAYERS: usize = 3;

#[derive(Clone, Copy, Debug)]
struct Write {
    layer: usize,
    position: IVec2,
    old: u8,
    new: u8,
}

fn offset(layer: usize, position: IVec2) -> Option<usize> {
    if position.x < 0 || position.y < 0 || position.x as usize >= WIDTH || position.y as usize >= HEIGHT {
        return None;
    }
    Some(layer * WIDTH * HEIGHT + position.y as usize * WIDTH + position.x as usize)
}

fn assert_matches(map: &LayerWeightMap, oracle: &[u8]) {
    for layer in 0..LAYERS {
        let plane = map.plane(layer as u32).expect("valid oracle layer");
        assert_eq!(
            plane,
            &oracle[layer * WIDTH * HEIGHT..(layer + 1) * WIDTH * HEIGHT]
        );
    }
    assert_eq!(map.is_blank(), oracle.iter().all(|weight| *weight == 0));
}

#[test]
fn interleaved_layer_views_revert_against_flat_oracle_across_reload() {
    let mut map = LayerWeightMap::new(WIDTH as u32, HEIGHT as u32, LAYERS as u32).expect("small map");
    let initial = map.to_bytes().expect("initial serialization");
    let mut oracle = vec![0_u8; WIDTH * HEIGHT * LAYERS];
    let mut log = Vec::new();

    // Exercise all four corners, every edge, negative coordinates, and points beyond the
    // rectangular grid. The fixed recurrence makes the generated bounded sequence repeatable.
    let mut state = 0x0051_a720_u32;
    let positions = [
        IVec2::new(0, 0),
        IVec2::new(WIDTH as i32 - 1, 0),
        IVec2::new(0, HEIGHT as i32 - 1),
        IVec2::new(WIDTH as i32 - 1, HEIGHT as i32 - 1),
        IVec2::new(2, 0),
        IVec2::new(0, 1),
        IVec2::new(WIDTH as i32 - 1, 1),
        IVec2::new(2, HEIGHT as i32 - 1),
        IVec2::new(-1, 0),
        IVec2::new(0, -1),
        IVec2::new(WIDTH as i32, 1),
        IVec2::new(2, HEIGHT as i32),
    ];

    for step in 0..96 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let layer = if step < LAYERS * 8 {
            step % LAYERS
        } else {
            (step + state as usize) % LAYERS
        };
        let position = if step < LAYERS * 8 {
            positions[(step / LAYERS) % 8]
        } else {
            positions[(state as usize >> 8) % positions.len()]
        };
        let value = (state >> 16) as u8;
        let cell = offset(layer, position);
        let old = cell.map(|i| oracle[i]);
        {
            let mut view = map.layer_mut(layer).expect("valid layer view");
            assert_eq!(view.size(), glam::UVec2::new(WIDTH as u32, HEIGHT as u32));
            assert_eq!(view.read(position), old);
            view.write(position, value);
            if let Some(i) = cell {
                oracle[i] = value;
            }
            assert_eq!(view.read(position), old.map(|_| value));
        }
        if let (Some(i), Some(old)) = (cell, old) {
            log.push(Write {
                layer,
                position,
                old,
                new: value,
            });
            assert_eq!(oracle[i], value);
        }
    }
    assert_matches(&map, &oracle);

    // Reacquired views must address the same serialized plane layout.
    let edited_bytes = map.to_bytes().expect("edited serialization");
    map = LayerWeightMap::from_bytes(&edited_bytes).expect("reload edited map");
    assert_matches(&map, &oracle);

    // Explicitly alternate two layer views after reload and ensure neither affects the other.
    for (layer, position, value) in [
        (0, IVec2::new(4, 2), 17),
        (2, IVec2::new(4, 2), 231),
        (0, IVec2::new(1, 2), 88),
        (2, IVec2::new(1, 2), 9),
    ] {
        let i = offset(layer, position).expect("valid alternating position");
        let old = oracle[i];
        let other_layer = if layer == 0 { 2 } else { 0 };
        let other = map.get(other_layer as u32, position.x as u32, position.y as u32);
        {
            let mut view = map.layer_mut(layer).expect("valid alternating view");
            assert_eq!(view.read(position), Some(old));
            view.write(position, value);
            assert_eq!(view.read(position), Some(value));
        }
        oracle[i] = value;
        log.push(Write {
            layer,
            position,
            old,
            new: value,
        });
        assert_eq!(
            map.get(other_layer as u32, position.x as u32, position.y as u32),
            other
        );
        assert_matches(&map, &oracle);
    }

    // A missing layer is rejected; the highest valid layer remains available.
    assert!(map.layer_mut(LAYERS).is_none());
    assert!(map.layer_mut(usize::MAX).is_none());
    assert!(map.layer_mut(LAYERS - 1).is_some());

    // Replay the recorded edit history backward, restoring each prior value through a newly
    // acquired view. Invalid-position writes were checked as no-ops and are not in the log.
    for write in log.iter().rev() {
        let i = offset(write.layer, write.position).expect("logged writes are in bounds");
        assert_eq!(oracle[i], write.new);
        {
            let mut view = map.layer_mut(write.layer).expect("logged layer is valid");
            assert_eq!(view.read(write.position), Some(write.new));
            view.write(write.position, write.old);
        }
        oracle[i] = write.old;
    }

    assert_matches(&map, &oracle);
    assert!(map.is_blank());
    assert_eq!(map.to_bytes().expect("reverted serialization"), initial);
}
