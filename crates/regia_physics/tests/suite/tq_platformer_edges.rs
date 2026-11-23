// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane: edge cases of the pixel-grid platformer world (`platformer2d`) that the unit
//! tests do not reach: non-finite deltas, degenerate layers, handle lifetime, `Tile::or`, repeated
//! runs.

use regia_core::math::IVec2;
use regia_physics::platformer2d::{PlatformerWorld, Tile};

/// A flat floor: one row of solid 8-px tiles, 10 wide, at y 0..8.
fn floor_world() -> PlatformerWorld {
    let mut world = PlatformerWorld::new();
    world.add_static_tiled_layer(vec![Tile::Solid; 10], 8, 8, 10, IVec2::ZERO, 1);
    world
}

#[test]
fn non_finite_deltas_do_not_poison_the_carried_fraction() {
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut world = floor_world();
        let actor = world.add_actor(IVec2::new(10, 8), 4, 4);
        assert!(
            world.move_h(actor, bad),
            "{bad}: a skipped move is not a blocked move"
        );
        assert!(world.move_v(actor, bad), "{bad}");
        assert_eq!(world.actor_pos(actor), Some(IVec2::new(10, 8)), "{bad}");
        let r = world.actor_remainder(actor).expect("remainder");
        assert!(r.x.is_finite() && r.y.is_finite(), "{bad}: remainder {r:?}");
        // The actor still moves afterwards: two 0.4 steps carry into one pixel.
        world.move_h(actor, 0.4);
        world.move_h(actor, 0.4);
        assert_eq!(world.actor_pos(actor), Some(IVec2::new(11, 8)), "{bad}");
    }
}

#[test]
fn a_finite_delta_that_overflows_the_remainder_is_ignored() {
    let mut world = floor_world();
    let actor = world.add_actor(IVec2::new(10, 8), 4, 4);
    // Cut to a bounded distance instead of looping 2^31 steps or overflowing the position.
    world.move_h(actor, f32::MAX);
    world.move_h(actor, f32::MAX);
    let r = world.actor_remainder(actor).expect("remainder");
    assert!(r.x.abs() <= 0.5, "remainder {r:?}");
    let pos = world.actor_pos(actor).expect("pos");
    assert!(pos.x > 10 && pos.x <= 10 + 3 * 1_048_576, "{pos:?}");
    world.move_v(actor, f32::MIN);
    // Past the end of the 80-px floor now, so it falls the capped distance and no further.
    assert_eq!(world.actor_pos(actor).expect("pos").y, 8 - 1_048_576);
}

#[test]
fn non_finite_solid_moves_neither_poison_nor_move_the_solid() {
    for bad in [f32::NAN, f32::INFINITY] {
        let mut world = floor_world();
        let solid = world.add_solid(IVec2::new(30, 20), 8, 4);
        let actor = world.add_actor(IVec2::new(30, 24), 4, 4);
        world.solid_move(solid, bad, bad);
        assert_eq!(world.solid_pos(solid), Some(IVec2::new(30, 20)), "{bad}");
        world.solid_move(solid, 1.0, 0.0);
        assert_eq!(world.solid_pos(solid), Some(IVec2::new(31, 20)), "{bad}");
        assert_eq!(
            world.actor_pos(actor),
            Some(IVec2::new(31, 24)),
            "{bad}: rider follows"
        );
    }
}

#[test]
fn set_actor_size_keeps_the_bottom_centre_even_for_non_positive_sizes() {
    let mut world = floor_world();
    let actor = world.add_actor(IVec2::new(20, 8), 8, 8);
    // A crouch: narrower, shorter; the centre column stays at x = 24.
    world.set_actor_size(actor, 4, 4);
    assert_eq!(world.actor_pos(actor), Some(IVec2::new(22, 8)));
    assert_eq!(world.actor_size(actor), Some((4, 4)));
    // A non-positive size clamps to one pixel and shifts as a one-pixel box would.
    world.set_actor_size(actor, -100, 0);
    assert_eq!(world.actor_size(actor), Some((1, 1)));
    assert_eq!(world.actor_pos(actor), Some(IVec2::new(22 + (4 - 1) / 2, 8)));
}

#[test]
fn set_position_and_size_clear_the_remainder() {
    let mut world = floor_world();
    let actor = world.add_actor(IVec2::new(10, 8), 4, 4);
    world.move_h(actor, 0.3);
    assert!(world.actor_remainder(actor).expect("r").x > 0.2);
    world.set_actor_position(actor, IVec2::new(50, 8));
    assert_eq!(world.actor_remainder(actor).expect("r").x, 0.0);
    world.move_h(actor, -0.3);
    world.set_actor_size(actor, 6, 6);
    assert_eq!(world.actor_remainder(actor).expect("r").x, 0.0);
}

#[test]
fn removed_handles_answer_safely_and_are_never_reused() {
    let mut world = floor_world();
    let a = world.add_actor(IVec2::new(10, 8), 4, 4);
    let s = world.add_solid(IVec2::new(40, 8), 4, 4);
    world.remove_actor(a);
    world.remove_solid(s);
    world.remove_actor(a); // twice is fine
    assert_eq!(world.actor_count(), 0);
    assert_eq!(world.solid_count(), 0);
    assert!(!world.move_h(a, 1.0));
    assert!(!world.move_v(a, 1.0));
    assert!(!world.collide_check(a, IVec2::ZERO));
    assert!(!world.squished(a));
    assert_eq!(world.actor_pos(a), None);
    assert_eq!(world.actor_remainder(a), None);
    assert_eq!(world.actor_size(a), None);
    assert_eq!(world.solid_pos(s), None);
    world.set_actor_position(a, IVec2::ONE);
    world.set_actor_size(a, 2, 2);
    world.descent(a);
    world.solid_move(s, 3.0, 3.0);
    let a2 = world.add_actor(IVec2::new(10, 8), 4, 4);
    assert_ne!(a, a2, "a removed handle is never reused");
    assert!(
        !world.solid_at(IVec2::new(40, 8)),
        "a removed solid no longer collides"
    );
}

#[test]
fn tile_or_is_solid_unless_every_part_is_one_way_or_empty() {
    use Tile::{Collider, Empty, JumpThrough, Solid};
    let all = [Empty, Solid, JumpThrough, Collider];
    for a in all {
        for b in all {
            let expect = match (a, b) {
                (Empty, Empty) => Empty,
                (JumpThrough | Empty, JumpThrough | Empty) => JumpThrough,
                _ => Solid,
            };
            assert_eq!(a.or(b), expect, "{a:?} or {b:?}");
            assert_eq!(a.or(b), b.or(a), "commutative: {a:?} {b:?}");
        }
    }
}

#[test]
fn degenerate_layers_read_as_empty_instead_of_panicking() {
    let mut world = PlatformerWorld::new();
    // width 0: no cells; tile sizes 0 and negative clamp to 1; a short last row has no tiles
    // past the end of the vector.
    world.add_static_tiled_layer(vec![Tile::Solid; 4], 8, 8, 0, IVec2::ZERO, 1);
    world.add_static_tiled_layer(vec![Tile::Solid; 3], 0, -5, 2, IVec2::new(100, 100), 1);
    world.add_static_tiled_layer(Vec::new(), 8, 8, 5, IVec2::ZERO, 1);
    assert!(!world.solid_at(IVec2::ZERO));
    // Layer 2 is 2 wide with one full row (x 100..102); the partial second row (one tile) is
    // not a row: the grid height is tiles / width, rounded down.
    assert!(world.solid_at(IVec2::new(100, 100)));
    assert!(world.solid_at(IVec2::new(101, 100)));
    assert!(
        !world.solid_at(IVec2::new(100, 101)),
        "the partial row is ignored"
    );
    assert!(!world.solid_at(IVec2::new(101, 101)));
    assert!(!world.solid_at(IVec2::new(102, 100)));
    assert!(!world.solid_at(IVec2::new(100, 99)));
    // Boxes of zero or negative size overlap nothing.
    assert_eq!(world.collide_solids(IVec2::new(100, 100), 0, 0), Tile::Empty);
    assert_eq!(world.collide_solids(IVec2::new(100, 100), -4, -4), Tile::Empty);
}

#[test]
fn tiles_left_of_and_below_the_origin_use_floor_division() {
    let mut world = PlatformerWorld::new();
    // One solid tile with its bottom-left at (-8, -8).
    world.add_static_tiled_layer(vec![Tile::Solid], 8, 8, 1, IVec2::new(-8, -8), 1);
    assert!(world.solid_at(IVec2::new(-8, -8)));
    assert!(world.solid_at(IVec2::new(-1, -1)));
    assert!(!world.solid_at(IVec2::new(0, 0)));
    assert!(!world.solid_at(IVec2::new(-9, -8)));
    assert!(!world.solid_at(IVec2::new(-8, -9)));
    assert_eq!(world.collide_solids(IVec2::new(-12, -12), 6, 6), Tile::Solid);
    assert_eq!(world.collide_solids(IVec2::new(-1, -1), 1, 1), Tile::Solid);
}

#[test]
fn layer_tags_separate_collision_from_other_layers() {
    let mut world = floor_world();
    world.add_static_tiled_layer(vec![Tile::Solid; 2], 8, 8, 2, IVec2::new(0, 16), 7);
    assert!(world.tag_at(IVec2::new(1, 17), 7));
    assert!(!world.tag_at(IVec2::new(1, 17), 1));
    assert!(!world.solid_at(IVec2::new(1, 17)), "tag 7 does not collide");
    assert_eq!(world.collide_tag(7, IVec2::new(1, 17), 2, 2), Tile::Solid);
    assert_eq!(world.collide_tag(9, IVec2::new(1, 17), 2, 2), Tile::Empty);
    assert_eq!(world.static_layers().len(), 2);
    world.clear_static_layers();
    assert!(world.static_layers().is_empty());
    assert!(!world.solid_at(IVec2::new(1, 1)));
}

#[test]
fn an_actor_spawned_inside_a_one_way_platform_drops_through_it() {
    let mut world = PlatformerWorld::new();
    let mut tiles = vec![Tile::Empty; 4];
    tiles[0] = Tile::JumpThrough;
    world.add_static_tiled_layer(tiles, 8, 8, 4, IVec2::new(0, 40), 1);
    let actor = world.add_actor(IVec2::new(2, 40), 4, 4);
    // Falling straight down: it is already inside the platform, so it keeps falling.
    assert!(world.move_v(actor, -20.0));
    assert_eq!(world.actor_pos(actor), Some(IVec2::new(2, 20)));
}

#[test]
fn a_large_fall_through_open_air_lands_exactly_on_the_floor() {
    let mut world = floor_world();
    let actor = world.add_actor(IVec2::new(10, 5000), 4, 4);
    assert!(!world.move_v(actor, -100_000.0), "the floor stops it");
    assert_eq!(world.actor_pos(actor), Some(IVec2::new(10, 8)));
    assert!(
        world.collide_check(actor, IVec2::new(10, 7)),
        "standing on the floor"
    );
}

/// A scripted run (gravity-style falls, sideways pushes, a moving solid) gives the same
/// positions twice.
#[test]
fn repeated_runs_agree() {
    fn run() -> Vec<(IVec2, bool)> {
        let mut world = floor_world();
        let solid = world.add_solid(IVec2::new(20, 12), 12, 4);
        let a = world.add_actor(IVec2::new(22, 16), 4, 6);
        let b = world.add_actor(IVec2::new(60, 60), 5, 5);
        let mut out = Vec::new();
        for i in 0..200 {
            let t = i as f32;
            world.solid_move(solid, (t * 0.1).sin() * 1.7, (t * 0.07).cos() * 0.9);
            world.move_h(a, 0.37);
            world.move_v(a, -0.61);
            world.move_h(b, -0.43);
            world.move_v(b, -1.13);
            out.push((world.actor_pos(a).expect("a"), world.squished(a)));
            out.push((world.actor_pos(b).expect("b"), world.squished(b)));
        }
        out
    }
    assert_eq!(run(), run());
}

#[test]
fn an_origin_shift_moves_tiles_actors_and_solids_together() {
    let mut world = floor_world();
    let actor = world.add_actor(IVec2::new(10, 8), 4, 4);
    let solid = world.add_solid(IVec2::new(30, 20), 8, 4);
    world.shift_origin(IVec2::new(1024, 0));
    assert_eq!(world.actor_pos(actor), Some(IVec2::new(10 - 1024, 8)));
    assert_eq!(world.solid_pos(solid), Some(IVec2::new(30 - 1024, 20)));
    // Still standing on the (shifted) floor: a drop is blocked, a step sideways stays on it.
    assert!(!world.move_v(actor, -3.0), "the floor tiles moved with the actor");
    assert!(world.move_h(actor, 5.0));
    assert_eq!(world.actor_pos(actor), Some(IVec2::new(15 - 1024, 8)));
}
