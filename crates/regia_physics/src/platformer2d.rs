// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from macroquad (https://github.com/not-fl3/macroquad, commit 8d602d3: physics-platformer/src/lib.rs),
// Copyright (c) 2019-2021 Fedor Logachev, MIT OR Apache-2.0, used under the MIT licence.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! Kinematic pixel platformer physics (`docs/specs/22-tier2.md` § T2.10): the "actors and solids"
//! model of Celeste-style games, ported from macroquad's `physics-platformer`.
//!
//! - **Actors** (players, enemies) move one whole pixel at a time with a sub-pixel remainder and
//!   stop at the first pixel that would overlap a solid: [`PlatformerWorld::move_h`] /
//!   [`PlatformerWorld::move_v`] return `false` on a hit.
//! - **Solids** (moving platforms, doors) move with [`PlatformerWorld::solid_move`]: actors in the
//!   way are pushed (and flagged squished when they cannot move), actors standing on top ride along.
//! - **Static tile layers** come from a tilemap's collision grid: [`Tile::Solid`] blocks from every
//!   side, [`Tile::JumpThrough`] (one-way) only blocks an actor falling onto it from above.
//!
//! Changes from the original: Regia Engine's world is **+Y up** (an actor's position is its
//! bottom-left pixel), positions are integers ([`IVec2`]), riders follow a solid on **both** axes
//! (macroquad only carried them horizontally), pushes and rides move by exact pixel counts (no
//! rounding through the remainder), tile cells are looked up with the right axis (the original
//! swapped tile width/height) and with `floor` (negative coordinates), every overlapped cell is
//! tested (the original sampled the perimeter, missing tiles inside large actors), static layers
//! have an origin, a drop-through request survives a frame whose move rounds to zero pixels, fast
//! solids push what they sweep instead of tunnelling through it, and actors/solids can be removed. On top sits an ECS layer:
//! [`PlatformerActor`] / [`PlatformerSolid`] components stepped in `FixedUpdate` by
//! [`PlatformerPlugin`].

use std::collections::{BTreeMap, BTreeSet, HashMap};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Actors examined by stationary or moving solid scans. `cargo test --lib` only.
#[cfg(test)]
pub(crate) static ACTOR_SCANS: AtomicUsize = AtomicUsize::new(0);

/// Solids examined by [`PlatformerWorld::collide_solids`]. `cargo test --lib` only.
#[cfg(test)]
pub(crate) static SOLID_VISITS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
thread_local! {
    /// Grid cells [`PlatformerWorld::collide_solids`] looked up on this thread (test only).
    static SOLID_CELL_LOOKUPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

use regia_app::prelude::*;
use regia_core::math::{IVec2, Vec2};
use regia_scene::Transform;
use regia_scene::layers::LayerParked;

/// What a tile (or a combination of tiles) is to an actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Reflect)]
pub enum Tile {
    /// Nothing.
    #[default]
    Empty,
    /// Blocks from every side.
    Solid,
    /// One-way platform: blocks only an actor falling onto it.
    JumpThrough,
    /// A moving solid (not a tile).
    Collider,
}

impl Tile {
    /// Combines two overlaps: one-way only when every part is one-way (or empty).
    #[must_use]
    pub fn or(self, other: Tile) -> Tile {
        match (self, other) {
            (Tile::Empty, Tile::Empty) => Tile::Empty,
            (Tile::JumpThrough, Tile::JumpThrough)
            | (Tile::JumpThrough, Tile::Empty)
            | (Tile::Empty, Tile::JumpThrough) => Tile::JumpThrough,
            _ => Tile::Solid,
        }
    }
}

/// A grid of static tiles, row 0 at the bottom.
#[derive(Clone, Debug, PartialEq)]
pub struct StaticTiledLayer {
    /// Tiles, row by row from the bottom (`width` per row).
    pub tiles: Vec<Tile>,
    /// Tile width in pixels.
    pub tile_width: i32,
    /// Tile height in pixels.
    pub tile_height: i32,
    /// Tiles per row.
    pub width: usize,
    /// World pixel of the grid's bottom-left corner.
    pub origin: IVec2,
    /// Layer tag (1 = collision; other tags are for [`PlatformerWorld::tag_at`] queries such as
    /// ladders or water).
    pub tag: u8,
}

impl StaticTiledLayer {
    fn height(&self) -> usize {
        self.tiles.len().checked_div(self.width).unwrap_or(0)
    }

    /// The tile under world pixel `p`.
    fn at(&self, p: IVec2) -> Tile {
        let local = p - self.origin;
        let cx = local.x.div_euclid(self.tile_width.max(1));
        let cy = local.y.div_euclid(self.tile_height.max(1));
        if cx < 0 || cy < 0 || cx as usize >= self.width || cy as usize >= self.height() {
            return Tile::Empty;
        }
        self.tiles[cy as usize * self.width + cx as usize]
    }

    /// The combined tile over a `w`×`h` box at `pos` (every overlapped cell).
    fn over(&self, pos: IVec2, w: i32, h: i32) -> Tile {
        let (tw, th) = (self.tile_width.max(1), self.tile_height.max(1));
        let lo = pos - self.origin;
        let hi = IVec2::new(lo.x.saturating_add(w - 1), lo.y.saturating_add(h - 1));
        let mut out = Tile::Empty;
        // Clamp the cell range to the layer first: a huge box must cost the layer size, not the
        // box size.
        let (cols, rows) = (self.width as i32, self.height() as i32);
        let cx_range = lo.x.div_euclid(tw).max(0)..=hi.x.div_euclid(tw).min(cols - 1);
        let cy_range = lo.y.div_euclid(th).max(0)..=hi.y.div_euclid(th).min(rows - 1);
        for cy in cy_range {
            for cx in cx_range.clone() {
                let tile = self.tiles[cy as usize * self.width + cx as usize];
                if tile != Tile::Empty {
                    out = out.or(tile);
                }
            }
        }
        out
    }
}

/// Largest width or height (pixels) of an actor or solid; larger sizes are clamped.
const MAX_BODY_SIZE: i32 = 4096;

/// Most pixels one move call may ask for.
const MAX_MOVE_PIXELS: f32 = 1_048_576.0;

/// Largest pixel coordinate taken from a `Transform`.
const MAX_WORLD_PIXEL: i32 = 1 << 30;

/// A move delta the world can act on: NaN and infinities become 0 (they would poison the carried
/// fraction for good) and huge values are cut to `MAX_MOVE_PIXELS` (a step loop of 2^31 pixels
/// would hang the frame).
fn sane_delta(d: f32) -> f32 {
    if d.is_finite() {
        d.clamp(-MAX_MOVE_PIXELS, MAX_MOVE_PIXELS)
    } else {
        0.0
    }
}

/// An actor handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Component)]
pub struct Actor(pub u32);

/// A solid handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Component)]
pub struct Solid(pub u32);

#[derive(Clone, Debug)]
struct Body {
    collidable: bool,
    squished: bool,
    pos: IVec2,
    width: i32,
    height: i32,
    remainder: Vec2,
    squishers: BTreeSet<Solid>,
    descent: bool,
    seen_wood: bool,
}

impl Body {
    fn new(pos: IVec2, width: i32, height: i32) -> Self {
        Self {
            collidable: true,
            squished: false,
            pos,
            width,
            height,
            remainder: Vec2::ZERO,
            squishers: BTreeSet::new(),
            descent: false,
            seen_wood: false,
        }
    }

    /// Half-open overlap of two boxes.
    fn overlaps(&self, pos: IVec2, w: i32, h: i32) -> bool {
        self.pos.x < pos.x.saturating_add(w)
            && pos.x < self.pos.x.saturating_add(self.width)
            && self.pos.y < pos.y.saturating_add(h)
            && pos.y < self.pos.y.saturating_add(self.height)
    }
}

/// The platformer world: static tile layers, solids and actors.
#[derive(Resource, Default, Debug)]
pub struct PlatformerWorld {
    layers: Vec<StaticTiledLayer>,
    /// Live solids by handle. A removed body leaves the map (audit v033b: removed slots used to stay
    /// as `None` forever, so the storage and every scan grew with each body ever spawned). Handles
    /// are still never reused, and the map iterates in handle order, the order the old slots had.
    solids: BTreeMap<u32, Body>,
    /// Live actors by handle (see `solids`).
    actors: BTreeMap<u32, Body>,
    /// The next solid handle.
    next_solid: u32,
    /// The next actor handle.
    next_actor: u32,
    /// Entities whose `PlatformerActor` left since the last fixed step, recorded by a `Remove`
    /// observer: removal messages are cleared every frame and would be lost on frames without a
    /// fixed step (ADR-1660).
    removed_actors: Vec<Entity>,
    /// The same for `PlatformerSolid`.
    removed_solids: Vec<Entity>,
    /// Origin shifts applied since `move_platformer_actors` last ran: its record of the pixel
    /// it wrote last moves by them, so a rebase is not read as gameplay teleporting the actor.
    pending_shift: IVec2,
    /// Set when an actor is marked squished, cleared when none remain. A stationary solid
    /// skips its actor scan unless this is set, because that scan is what clears the flag.
    any_squished: bool,
    /// Actor handles retained across solid moves.
    solid_actor_ids: Vec<u32>,
    /// Riders of the solid currently moving, in handle order, retained across moves.
    solid_riders: Vec<Actor>,
    /// Solid handles per [`SOLID_CELL`] pixels. Empty cells are removed.
    solid_cells: BTreeMap<(i32, i32), Vec<u32>>,
}

/// Pixel size of one solid-grid cell.
const SOLID_CELL: i32 = 64;

fn solid_cell_range(pos: IVec2, w: i32, h: i32) -> (i32, i32, i32, i32) {
    let last_x = pos.x.saturating_add(w).saturating_sub(1);
    let last_y = pos.y.saturating_add(h).saturating_sub(1);
    let mut x0 = pos.x.div_euclid(SOLID_CELL);
    let mut y0 = pos.y.div_euclid(SOLID_CELL);
    let mut x1 = last_x.div_euclid(SOLID_CELL);
    let mut y1 = last_y.div_euclid(SOLID_CELL);
    if x1 < x0 {
        std::mem::swap(&mut x0, &mut x1);
    }
    if y1 < y0 {
        std::mem::swap(&mut y0, &mut y1);
    }
    (x0, y0, x1, y1)
}

impl PlatformerWorld {
    /// An empty world.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves the whole world by `-delta` pixels after a floating-origin rebase: every tile layer
    /// origin and every solid and actor position. Velocities and sub-pixel remainders stay.
    pub fn shift_origin(&mut self, delta: IVec2) {
        for layer in &mut self.layers {
            layer.origin -= delta;
        }
        for body in self.solids.values_mut().chain(self.actors.values_mut()) {
            body.pos -= delta;
        }
        self.pending_shift += delta;
        self.rebuild_solid_cells();
    }

    /// Adds a static tile layer (row 0 at the bottom, `origin` = its bottom-left world pixel).
    pub fn add_static_tiled_layer(
        &mut self,
        tiles: Vec<Tile>,
        tile_width: i32,
        tile_height: i32,
        width: usize,
        origin: IVec2,
        tag: u8,
    ) {
        self.layers.push(StaticTiledLayer {
            tiles,
            tile_width,
            tile_height,
            width,
            origin,
            tag,
        });
    }

    /// Removes every static layer (before loading another map).
    pub fn clear_static_layers(&mut self) {
        self.layers.clear();
    }

    /// The static layers.
    #[must_use]
    pub fn static_layers(&self) -> &[StaticTiledLayer] {
        &self.layers
    }

    /// Adds an actor of `width`×`height` pixels with its bottom-left at `pos`. An actor spawned
    /// inside a one-way platform starts dropping through it.
    pub fn add_actor(&mut self, pos: IVec2, width: i32, height: i32) -> Actor {
        let actor = Actor(self.next_actor);
        self.next_actor = self.next_actor.wrapping_add(1);
        let mut body = Body::new(pos, width.clamp(1, MAX_BODY_SIZE), height.clamp(1, MAX_BODY_SIZE));
        if self.collide_solids(pos, body.width, body.height) == Tile::JumpThrough {
            body.descent = true;
            body.seen_wood = true;
        }
        self.actors.insert(actor.0, body);
        actor
    }

    /// Adds a solid of `width`×`height` pixels with its bottom-left at `pos`.
    pub fn add_solid(&mut self, pos: IVec2, width: i32, height: i32) -> Solid {
        let solid = Solid(self.next_solid);
        self.next_solid = self.next_solid.wrapping_add(1);
        self.solids.insert(
            solid.0,
            Body::new(pos, width.clamp(1, MAX_BODY_SIZE), height.clamp(1, MAX_BODY_SIZE)),
        );
        self.index_solid(solid.0);
        solid
    }

    /// Removes an actor (its handle is never reused).
    pub fn remove_actor(&mut self, actor: Actor) {
        let was_squished = self.actors.get(&actor.0).is_some_and(|a| a.squished);
        self.actors.remove(&actor.0);
        if was_squished {
            self.any_squished = self.actors.values().any(|a| a.squished);
        }
    }

    /// Removes a solid (its handle is never reused).
    pub fn remove_solid(&mut self, solid: Solid) {
        if let Some((pos, w, h)) = self.solids.get(&solid.0).map(|b| (b.pos, b.width, b.height)) {
            self.unindex_solid(solid.0, pos, w, h);
        }
        self.solids.remove(&solid.0);
    }

    fn index_solid(&mut self, handle: u32) {
        let Some((pos, w, h)) = self.solids.get(&handle).map(|b| (b.pos, b.width, b.height)) else {
            return;
        };
        let (x0, y0, x1, y1) = solid_cell_range(pos, w, h);
        for y in y0..=y1 {
            for x in x0..=x1 {
                self.solid_cells.entry((x, y)).or_default().push(handle);
            }
        }
    }

    fn unindex_solid(&mut self, handle: u32, pos: IVec2, w: i32, h: i32) {
        let (x0, y0, x1, y1) = solid_cell_range(pos, w, h);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let Some(list) = self.solid_cells.get_mut(&(x, y)) else {
                    continue;
                };
                list.retain(|h| *h != handle);
                if list.is_empty() {
                    self.solid_cells.remove(&(x, y));
                }
            }
        }
    }

    fn rebuild_solid_cells(&mut self) {
        self.solid_cells.clear();
        let handles: Vec<u32> = self.solids.keys().copied().collect();
        for handle in handles {
            self.index_solid(handle);
        }
    }

    fn clear_solid_squish(&mut self, solid: Solid, spos: IVec2, sw: i32, sh: i32) {
        let mut still = false;
        for actor in self.actors.values_mut() {
            if !actor.overlaps(spos, sw, sh) {
                actor.squishers.remove(&solid);
                if actor.squishers.is_empty() {
                    actor.squished = false;
                }
            }
            if actor.squished {
                still = true;
            }
        }
        self.any_squished = still;
    }

    fn riding(&self, actor: Actor) -> bool {
        self.solid_riders.binary_search(&actor).is_ok()
    }

    fn actor(&self, actor: Actor) -> Option<&Body> {
        self.actors.get(&actor.0)
    }

    fn actor_mut(&mut self, actor: Actor) -> Option<&mut Body> {
        self.actors.get_mut(&actor.0)
    }

    /// Resizes an actor keeping its bottom centre (crouching), and clears its remainder.
    pub fn set_actor_size(&mut self, actor: Actor, width: i32, height: i32) {
        if let Some(body) = self.actor_mut(actor) {
            let (width, height) = (width.clamp(1, MAX_BODY_SIZE), height.clamp(1, MAX_BODY_SIZE));
            body.pos.x += (body.width - width) / 2;
            body.width = width;
            body.height = height;
            body.remainder = Vec2::ZERO;
        }
    }

    /// Teleports an actor and clears its remainder.
    pub fn set_actor_position(&mut self, actor: Actor, pos: IVec2) {
        if let Some(body) = self.actor_mut(actor) {
            body.pos = pos;
            body.remainder = Vec2::ZERO;
        }
    }

    /// Lets the actor drop through the one-way platform it stands on.
    pub fn descent(&mut self, actor: Actor) {
        if let Some(body) = self.actor_mut(actor) {
            body.descent = true;
        }
    }

    /// Moves an actor vertically by `dy` pixels (+ up), carrying the fraction to the next call.
    /// Returns false when a solid (or a one-way platform below a falling actor) stopped it.
    pub fn move_v(&mut self, actor: Actor, dy: f32) -> bool {
        let Some(body) = self.actor_mut(actor) else {
            return false;
        };
        body.remainder.y += sane_delta(dy);
        let steps = body.remainder.y.round() as i32;
        body.remainder.y -= steps as f32;
        self.step_v(actor, steps)
    }

    /// Moves an actor horizontally by `dx` pixels, carrying the fraction to the next call.
    /// Returns false when a solid stopped it (one-way platforms never block sideways).
    pub fn move_h(&mut self, actor: Actor, dx: f32) -> bool {
        let Some(body) = self.actor_mut(actor) else {
            return false;
        };
        body.remainder.x += sane_delta(dx);
        let steps = body.remainder.x.round() as i32;
        body.remainder.x -= steps as f32;
        self.step_h(actor, steps)
    }

    /// Moves an actor exactly `steps` whole pixels vertically (no remainder).
    fn step_v(&mut self, actor: Actor, mut steps: i32) -> bool {
        let Some(mut c) = self.actor(actor).cloned() else {
            return false;
        };
        let sign = steps.signum();
        let moving = steps != 0;
        let mut free = true;
        while steps != 0 {
            let tile = self.collide_solids(c.pos + IVec2::new(0, sign), c.width, c.height);
            if tile == Tile::JumpThrough && c.descent {
                c.seen_wood = true;
            }
            // Moving up through a one-way platform: pass, and keep passing until clear of it.
            if tile == Tile::JumpThrough && sign > 0 {
                c.seen_wood = true;
                c.descent = true;
            }
            if tile == Tile::Empty || (tile == Tile::JumpThrough && c.descent) {
                c.pos.y += sign;
                steps -= sign;
            } else {
                free = false;
                break;
            }
        }
        // Out of the one-way platform after a move: forget the drop-through. (Only after a real
        // move: the original also reset it when the move rounded to zero pixels, which cancelled a
        // drop request made while standing still on the platform.)
        if free && moving && self.collide_solids(c.pos, c.width, c.height) != Tile::JumpThrough {
            c.seen_wood = false;
            c.descent = false;
        }
        if let Some(body) = self.actor_mut(actor) {
            *body = c;
        }
        free
    }

    /// Moves an actor exactly `steps` whole pixels horizontally (no remainder).
    fn step_h(&mut self, actor: Actor, mut steps: i32) -> bool {
        let Some(mut c) = self.actor(actor).cloned() else {
            return false;
        };
        let sign = steps.signum();
        let mut free = true;
        while steps != 0 {
            let tile = self.collide_solids(c.pos + IVec2::new(sign, 0), c.width, c.height);
            if tile == Tile::JumpThrough {
                c.descent = true;
                c.seen_wood = true;
            }
            if tile == Tile::Empty || tile == Tile::JumpThrough {
                c.pos.x += sign;
                steps -= sign;
            } else {
                free = false;
                break;
            }
        }
        if let Some(body) = self.actor_mut(actor) {
            *body = c;
        }
        free
    }

    /// Moves a solid by `(dx, dy)` pixels (fractions carried). Actors it runs into are pushed
    /// (an actor that cannot move is flagged [`Self::squished`]); actors standing on it ride along
    /// on both axes.
    pub fn solid_move(&mut self, solid: Solid, dx: f32, dy: f32) {
        let Some(body) = self.solids.get_mut(&solid.0) else {
            return;
        };
        body.remainder += Vec2::new(sane_delta(dx), sane_delta(dy));
        let move_x = body.remainder.x.round() as i32;
        let move_y = body.remainder.y.round() as i32;
        body.remainder -= Vec2::new(move_x as f32, move_y as f32);
        let (spos, sw, sh) = (body.pos, body.width, body.height);
        let start_pos = spos;
        // Remainder is already stored. A solid that will not move, and that is not crushing
        // anyone, has nothing to scan.
        if move_x == 0 && move_y == 0 {
            if self.any_squished {
                self.clear_solid_squish(solid, spos, sw, sh);
            }
            return;
        }

        #[cfg(test)]
        ACTOR_SCANS.fetch_add(self.actors.len(), Ordering::Relaxed);
        self.solid_riders.clear();
        for (&i, actor) in &self.actors {
            let on_top =
                actor.pos.y == spos.y + sh && actor.pos.x < spos.x + sw && spos.x < actor.pos.x + actor.width;
            if on_top {
                self.solid_riders.push(Actor(i));
            }
        }
        self.clear_solid_squish(solid, spos, sw, sh);

        self.set_solid_collidable(solid, false);
        for (axis, amount) in [(0, move_x), (1, move_y)] {
            if amount == 0 {
                continue;
            }
            let Some(body) = self.solids.get_mut(&solid.0) else {
                return;
            };
            let old = body.pos;
            if axis == 0 {
                body.pos.x += amount;
            } else {
                body.pos.y += amount;
            }
            let (spos, sw, sh) = (body.pos, body.width, body.height);
            // The area the solid swept this move: a fast solid pushes what it passed, it does not
            // tunnel through it.
            let (sweep_pos, sweep_w, sweep_h) = if axis == 0 {
                (IVec2::new(old.x.min(spos.x), spos.y), sw + amount.abs(), sh)
            } else {
                (IVec2::new(spos.x, old.y.min(spos.y)), sw, sh + amount.abs())
            };
            self.solid_actor_ids.clear();
            self.solid_actor_ids.extend(self.actors.keys().copied());
            let mut actor_i = 0;
            while actor_i < self.solid_actor_ids.len() {
                let i = self.solid_actor_ids[actor_i];
                actor_i += 1;
                let actor = Actor(i);
                let Some(a) = self.actor(actor) else { continue };
                if a.overlaps(sweep_pos, sweep_w, sweep_h) && !self.riding(actor) || a.overlaps(spos, sw, sh)
                {
                    if a.squished {
                        continue;
                    }
                    // Push out of the solid's new rectangle along the moving axis.
                    let push = match (axis, amount > 0) {
                        (0, true) => spos.x + sw - a.pos.x,
                        (0, false) => spos.x - (a.pos.x + a.width),
                        (_, true) => spos.y + sh - a.pos.y,
                        (_, false) => spos.y - (a.pos.y + a.height),
                    };
                    let moved = if axis == 0 {
                        self.step_h(actor, push)
                    } else {
                        self.step_v(actor, push)
                    };
                    if !moved {
                        if let Some(a) = self.actor_mut(actor) {
                            a.squished = true;
                            a.squishers.insert(solid);
                        }
                        self.any_squished = true;
                    }
                } else if self.riding(actor) {
                    let carried = if axis == 0 {
                        self.step_h(actor, amount)
                    } else {
                        self.step_v(actor, amount)
                    };
                    // A rider blocked while the solid keeps coming is crushed against whatever
                    // stopped it (a ceiling); one merely left behind is not.
                    if !carried && self.actor(actor).is_some_and(|a| a.overlaps(spos, sw, sh)) {
                        if let Some(a) = self.actor_mut(actor) {
                            a.squished = true;
                            a.squishers.insert(solid);
                        }
                        self.any_squished = true;
                    }
                }
            }
        }
        self.set_solid_collidable(solid, true);
        if let Some((pos, w, h)) = self.solids.get(&solid.0).map(|b| (b.pos, b.width, b.height))
            && pos != start_pos
        {
            self.unindex_solid(solid.0, start_pos, w, h);
            self.index_solid(solid.0);
        }
    }

    fn set_solid_collidable(&mut self, solid: Solid, collidable: bool) {
        if let Some(body) = self.solids.get_mut(&solid.0) {
            body.collidable = collidable;
        }
    }

    /// True when a collision-layer tile or a solid covers world pixel `pos`.
    #[must_use]
    pub fn solid_at(&self, pos: IVec2) -> bool {
        self.tag_at(pos, 1) || self.solid_overlaps(pos, 1, 1)
    }

    /// True when a non-empty tile of a layer tagged `tag` covers world pixel `pos`.
    #[must_use]
    pub fn tag_at(&self, pos: IVec2, tag: u8) -> bool {
        self.layers
            .iter()
            .any(|l| l.tag == tag && l.at(pos) != Tile::Empty)
    }

    /// What a `w`×`h` box at `pos` overlaps: collision tiles first, then solids
    /// ([`Tile::Collider`]).
    #[must_use]
    pub fn collide_solids(&self, pos: IVec2, w: i32, h: i32) -> Tile {
        let tile = self.collide_tag(1, pos, w, h);
        if tile != Tile::Empty {
            return tile;
        }
        if self.solid_overlaps(pos, w, h) {
            Tile::Collider
        } else {
            Tile::Empty
        }
    }

    fn solid_overlaps(&self, pos: IVec2, w: i32, h: i32) -> bool {
        let (x0, y0, x1, y1) = solid_cell_range(pos, w, h);
        let hit = |list: &Vec<u32>| {
            list.iter().any(|handle| {
                let Some(solid) = self.solids.get(handle) else {
                    return false;
                };
                #[cfg(test)]
                SOLID_VISITS.fetch_add(1, Ordering::Relaxed);
                solid.collidable && solid.overlaps(pos, w, h)
            })
        };
        // A box spanning more cells than hold solids walks the occupied cells instead: the cost
        // must follow the solids, not the box (corr-physics audit: a 2^24-pixel box was about
        // 7e10 lookups). The answer is a yes/no, so the walk order does not matter.
        let span = (i64::from(x1) - i64::from(x0) + 1) * (i64::from(y1) - i64::from(y0) + 1);
        if span > self.solid_cells.len() as i64 {
            return self.solid_cells.iter().any(|(&(x, y), list)| {
                #[cfg(test)]
                SOLID_CELL_LOOKUPS.with(|n| n.set(n.get() + 1));
                (x0..=x1).contains(&x) && (y0..=y1).contains(&y) && hit(list)
            });
        }
        for y in y0..=y1 {
            for x in x0..=x1 {
                #[cfg(test)]
                SOLID_CELL_LOOKUPS.with(|n| n.set(n.get() + 1));
                if self.solid_cells.get(&(x, y)).is_some_and(hit) {
                    return true;
                }
            }
        }
        false
    }

    /// The combined tile of the layers tagged `tag` over a `w`×`h` box at `pos`.
    #[must_use]
    pub fn collide_tag(&self, tag: u8, pos: IVec2, w: i32, h: i32) -> Tile {
        self.layers
            .iter()
            .filter(|l| l.tag == tag)
            .fold(Tile::Empty, |acc, l| match l.over(pos, w, h) {
                Tile::Empty => acc,
                t if acc == Tile::Empty => t,
                t => acc.or(t),
            })
    }

    /// Whether an actor would collide at `pos` (a one-way platform counts unless the actor is
    /// dropping through it). `collide_check(actor, pos - (0, 1))` is "standing on something".
    #[must_use]
    pub fn collide_check(&self, actor: Actor, pos: IVec2) -> bool {
        let Some(a) = self.actor(actor) else {
            return false;
        };
        let tile = self.collide_solids(pos, a.width, a.height);
        if a.descent {
            matches!(tile, Tile::Solid | Tile::Collider)
        } else {
            tile != Tile::Empty
        }
    }

    /// Whether a solid pushed the actor into something.
    #[must_use]
    pub fn squished(&self, actor: Actor) -> bool {
        self.actor(actor).is_some_and(|a| a.squished)
    }

    /// The actor's bottom-left pixel.
    #[must_use]
    pub fn actor_pos(&self, actor: Actor) -> Option<IVec2> {
        self.actor(actor).map(|a| a.pos)
    }

    /// The actor's sub-pixel remainder (always within ±0.5).
    #[must_use]
    pub fn actor_remainder(&self, actor: Actor) -> Option<Vec2> {
        self.actor(actor).map(|a| a.remainder)
    }

    /// The actor's size in pixels.
    #[must_use]
    pub fn actor_size(&self, actor: Actor) -> Option<(i32, i32)> {
        self.actor(actor).map(|a| (a.width, a.height))
    }

    /// The solid's bottom-left pixel.
    #[must_use]
    pub fn solid_pos(&self, solid: Solid) -> Option<IVec2> {
        self.solids.get(&solid.0).map(|s| s.pos)
    }

    /// Live actors.
    #[must_use]
    pub fn actor_count(&self) -> usize {
        self.actors.len()
    }

    /// Live solids.
    #[must_use]
    pub fn solid_count(&self) -> usize {
        self.solids.len()
    }
}

// -------------------------------------------------------------------------------------------
// ECS layer
// -------------------------------------------------------------------------------------------

/// A platformer actor: a pixel box (bottom-left at the entity's `Transform` x, y) moved by its
/// velocity and gravity on the fixed step.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
#[require(Transform, PlatformerContacts)]
pub struct PlatformerActor {
    /// Size in pixels.
    pub size: IVec2,
    /// Velocity in pixels per second (+Y up).
    pub velocity: Vec2,
    /// Downward acceleration in pixels per second squared.
    pub gravity: f32,
    /// Largest downward speed in pixels per second.
    pub max_fall_speed: f32,
    /// Set to drop through the one-way platform underneath (cleared after the step).
    pub drop_through: bool,
}

impl Default for PlatformerActor {
    fn default() -> Self {
        Self {
            size: IVec2::new(8, 8),
            velocity: Vec2::ZERO,
            gravity: 0.0,
            max_fall_speed: 1.0e4,
            drop_through: false,
        }
    }
}

/// What the last fixed step did to a [`PlatformerActor`].
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct PlatformerContacts {
    /// Standing on a tile, a one-way platform or a solid.
    pub grounded: bool,
    /// A horizontal move was stopped.
    pub hit_wall: bool,
    /// An upward move was stopped.
    pub hit_ceiling: bool,
    /// A solid pushed it into something.
    pub squished: bool,
}

/// A platformer solid (moving platform): a pixel box moved by its velocity on the fixed step,
/// pushing and carrying actors.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
#[require(Transform)]
pub struct PlatformerSolid {
    /// Size in pixels.
    pub size: IVec2,
    /// Velocity in pixels per second.
    pub velocity: Vec2,
}

impl Default for PlatformerSolid {
    fn default() -> Self {
        Self {
            size: IVec2::new(16, 16),
            velocity: Vec2::ZERO,
        }
    }
}

/// The platformer chain in `FixedUpdate`: register → move solids → move actors.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlatformerStep;

/// Steps [`PlatformerActor`]s and [`PlatformerSolid`]s on the fixed step against the
/// [`PlatformerWorld`] resource (whose static tile layers the game fills from its tilemap).
#[derive(Default)]
pub struct PlatformerPlugin;

impl Plugin for PlatformerPlugin {
    fn build(&self, app: &mut App) {
        // Fixed order against the other `Transform` writers (3D physics, then 2D physics).
        app.configure_sets(FixedUpdate, PlatformerStep.after(crate::PhysicsStep));
        app.init_resource::<PlatformerWorld>()
            .add_observer(record_actor_removal)
            .add_observer(record_solid_removal)
            .register_type::<PlatformerActor>()
            .register_type::<PlatformerSolid>()
            .register_type::<PlatformerContacts>()
            .add_systems(
                FixedUpdate,
                (
                    park_platformer_bodies,
                    register_platformer_bodies,
                    move_platformer_solids,
                    move_platformer_actors,
                )
                    .chain()
                    .in_set(PlatformerStep),
            )
            .add_message::<regia_scene::streaming::OriginShifted>()
            .add_systems(
                FixedUpdate,
                shift_platformer_origin
                    .after(regia_scene::streaming::StreamSet::Rebase)
                    .before(regia_scene::streaming::StreamSet::Plan),
            );
        // Handles follow the order bodies arrive, not entity ids (ADR-1347, ADR-2396).
        crate::plugin::must_track_spawn_order::<PlatformerActor>(app.world_mut());
        crate::plugin::must_track_spawn_order::<PlatformerSolid>(app.world_mut());
    }
}

/// A floating-origin rebase moves the tiles, solids and actors by the same pixels as the
/// `Transform`s (the world is in `Transform` units).
fn shift_platformer_origin(
    mut shifted: MessageReader<regia_scene::streaming::OriginShifted>,
    mut world: ResMut<PlatformerWorld>,
) {
    for message in shifted.read() {
        let shift = message.rebase.shift;
        let axis = |v: f32| (v.round() as i32).clamp(-MAX_WORLD_PIXEL, MAX_WORLD_PIXEL);
        world.shift_origin(IVec2::new(axis(shift[0]), axis(shift[1])));
    }
}

/// The pixel under a `Transform`, kept far enough from `i32::MAX` that a body's edges and a whole
/// move still fit.
fn pixel(transform: &Transform) -> IVec2 {
    let axis = |v: f32| (v.round() as i32).clamp(-MAX_WORLD_PIXEL, MAX_WORLD_PIXEL);
    IVec2::new(axis(transform.translation.x), axis(transform.translation.y))
}

/// Observer: a `PlatformerActor` is leaving its entity (removal or despawn).
fn record_actor_removal(on: On<Remove<PlatformerActor>>, world: Option<ResMut<PlatformerWorld>>) {
    if let Some(mut world) = world {
        world.removed_actors.push(on.event().entity);
    }
}

/// Observer: a `PlatformerSolid` is leaving its entity (removal or despawn).
fn record_solid_removal(on: On<Remove<PlatformerSolid>>, world: Option<ResMut<PlatformerWorld>>) {
    if let Some(mut world) = world {
        world.removed_solids.push(on.event().entity);
    }
}

/// `FixedUpdate`, before [`register_platformer_bodies`]: queues the bodies of parked data-layer
/// members for removal (spec 37 § 13). Their components stay; the registration adds them back
/// once they are live.
#[allow(clippy::type_complexity)]
pub fn park_platformer_bodies(
    mut world: ResMut<PlatformerWorld>,
    actors: Query<Entity, (With<Actor>, With<LayerParked>)>,
    solids: Query<Entity, (With<Solid>, With<LayerParked>)>,
) {
    world.removed_actors.extend(actors.iter());
    world.removed_solids.extend(solids.iter());
}

/// `FixedUpdate`: adds new actors/solids to the world and removes despawned ones. Removals come
/// from the records the plugin's `Remove` observers keep, so one on a frame without a fixed step
/// is not lost.
#[allow(clippy::type_complexity)] // two body kinds × (new, handles)
#[tracing::instrument(skip_all, target = "regia_physics")]
pub fn register_platformer_bodies(
    mut commands: Commands,
    mut world: ResMut<PlatformerWorld>,
    new_actors: Query<(Entity, &PlatformerActor, &Transform), (Without<Actor>, Without<LayerParked>)>,
    new_solids: Query<(Entity, &PlatformerSolid, &Transform), (Without<Solid>, Without<LayerParked>)>,
    (actor_handles, solid_handles): (Query<&Actor>, Query<&Solid>),
    mut owned: Local<(HashMap<Entity, Actor>, HashMap<Entity, Solid>)>,
    orders: (
        Option<Res<regia_ecs::spawn_order::SpawnOrder<PlatformerActor>>>,
        Option<Res<regia_ecs::spawn_order::SpawnOrder<PlatformerSolid>>>,
    ),
) {
    let _span = tracing::debug_span!("register_platformer_bodies").entered();
    // A removed component leaves the handle on the entity; a despawn takes it along. The local
    // entity → handle maps release the body in both cases.
    let (actors_of, solids_of) = &mut *owned;
    let removed_actors = std::mem::take(&mut world.removed_actors);
    let removed_solids = std::mem::take(&mut world.removed_solids);
    for entity in removed_actors {
        if let Some(actor) = actors_of.remove(&entity) {
            world.remove_actor(actor);
            if actor_handles.get(entity).is_ok() {
                commands.entity(entity).remove::<Actor>();
            }
        }
    }
    for entity in removed_solids {
        if let Some(solid) = solids_of.remove(&entity) {
            world.remove_solid(solid);
            if solid_handles.get(entity).is_ok() {
                commands.entity(entity).remove::<Solid>();
            }
        }
    }
    // Handles are minted in the order the bodies arrived (`regia_ecs::spawn_order`), not in
    // archetype order and not in entity-id order (thread timing under parallel
    // `Commands::spawn`), so two runs number the bodies the same way on any thread count
    // (ADR-1347, ADR-2396). Entity ids are the fallback when tracking is off.
    let (actor_order, solid_order) = orders;
    let mut new_actors: Vec<_> = new_actors.iter().collect();
    new_actors.sort_by_cached_key(|row| crate::step::spawn_key(actor_order.as_deref(), row.0));
    let mut new_solids: Vec<_> = new_solids.iter().collect();
    new_solids.sort_by_cached_key(|row| crate::step::spawn_key(solid_order.as_deref(), row.0));
    for (entity, actor, transform) in new_actors {
        let handle = world.add_actor(pixel(transform), actor.size.x, actor.size.y);
        actors_of.insert(entity, handle);
        commands.entity(entity).insert(handle);
    }
    for (entity, solid, transform) in new_solids {
        let handle = world.add_solid(pixel(transform), solid.size.x, solid.size.y);
        solids_of.insert(entity, handle);
        commands.entity(entity).insert(handle);
    }
}

/// `FixedUpdate`: moves every solid by its velocity (pushing and carrying actors).
#[tracing::instrument(skip_all, target = "regia_physics")]
pub fn move_platformer_solids(
    fixed: Res<FixedTime>,
    mut world: ResMut<PlatformerWorld>,
    mut solids: Query<(&Solid, &PlatformerSolid, &mut Transform)>,
) {
    let _span = tracing::debug_span!("move_platformer_solids").entered();
    let dt = fixed.step().as_secs_f32();
    let mut order: Vec<_> = solids.iter_mut().collect();
    order.sort_by_key(|(s, _, _)| **s);
    for (solid, body, mut transform) in order {
        world.solid_move(*solid, body.velocity.x * dt, body.velocity.y * dt);
        if let Some(pos) = world.solid_pos(*solid) {
            transform.translation.x = pos.x as f32;
            transform.translation.y = pos.y as f32;
        }
    }
}

/// `FixedUpdate`: applies gravity, moves every actor (horizontal, then vertical), zeroes the
/// velocity component that hit something and writes [`PlatformerContacts`] and the `Transform`.
#[tracing::instrument(skip_all, target = "regia_physics")]
pub fn move_platformer_actors(
    fixed: Res<FixedTime>,
    mut world: ResMut<PlatformerWorld>,
    mut actors: Query<(
        &Actor,
        &mut PlatformerActor,
        &mut PlatformerContacts,
        &mut Transform,
    )>,
    mut written: Local<HashMap<Actor, IVec2>>,
) {
    let _span = tracing::debug_span!("move_platformer_actors").entered();
    let dt = fixed.step().as_secs_f32();
    // Forget the actors that left the world (audit v033b: one entry per actor ever moved stayed).
    written.retain(|actor, _| world.actor_size(*actor).is_some());
    // A rebase moved the `Transform`s: the pixels this system wrote move with them.
    let rebased = std::mem::take(&mut world.pending_shift);
    if rebased != IVec2::ZERO {
        for last in written.values_mut() {
            *last -= rebased;
        }
    }
    let mut order: Vec<_> = actors.iter_mut().collect();
    order.sort_by_key(|(a, _, _, _)| **a);
    for (actor, mut body, mut contacts, mut transform) in order {
        let actor = *actor;
        // Gameplay resized the actor (crouch): the world body follows.
        let size = (
            body.size.x.clamp(1, MAX_BODY_SIZE),
            body.size.y.clamp(1, MAX_BODY_SIZE),
        );
        if world.actor_size(actor).is_some_and(|current| current != size) {
            world.set_actor_size(actor, size.0, size.1);
        }
        // Gameplay moved the Transform since this system last wrote it: teleport the body.
        let requested = pixel(&transform);
        if written.get(&actor).is_some_and(|last| *last != requested) {
            world.set_actor_position(actor, requested);
        }
        if body.drop_through {
            world.descent(actor);
            body.drop_through = false;
        }
        body.velocity.y = (body.velocity.y - body.gravity * dt).max(-body.max_fall_speed);
        let hit_wall = !world.move_h(actor, body.velocity.x * dt);
        if hit_wall {
            body.velocity.x = 0.0;
        }
        let going_up = body.velocity.y > 0.0;
        let hit_v = !world.move_v(actor, body.velocity.y * dt);
        if hit_v {
            body.velocity.y = 0.0;
        }
        let pos = world.actor_pos(actor).unwrap_or_default();
        *contacts = PlatformerContacts {
            grounded: world.collide_check(actor, pos - IVec2::Y),
            hit_wall,
            hit_ceiling: hit_v && going_up,
            squished: world.squished(actor),
        };
        transform.translation.x = pos.x as f32;
        transform.translation.y = pos.y as f32;
        written.insert(actor, pos);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    /// A 10×6 grid of 8-px tiles: a solid floor (row 0), a solid wall at column 9, and a one-way
    /// platform at row 3, columns 2..=5.
    fn world() -> PlatformerWorld {
        let (w, h) = (10usize, 6usize);
        let mut tiles = vec![Tile::Empty; w * h];
        for tile in tiles.iter_mut().take(w) {
            *tile = Tile::Solid;
        }
        for y in 0..h {
            tiles[y * w + 9] = Tile::Solid;
        }
        for x in 2..=5 {
            tiles[3 * w + x] = Tile::JumpThrough;
        }
        let mut world = PlatformerWorld::new();
        world.add_static_tiled_layer(tiles, 8, 8, w, IVec2::ZERO, 1);
        world
    }

    #[test]
    fn a_huge_solid_query_costs_the_solids_not_the_box() {
        // Corr-physics audit: `collide_solids` walked every 64-pixel cell of the query box.
        let mut world = PlatformerWorld::new();
        world.add_solid(IVec2::new(0, 0), 8, 8);
        world.add_solid(IVec2::new(1 << 20, 1 << 20), 8, 8);
        SOLID_CELL_LOOKUPS.with(|n| n.set(0));
        assert_eq!(
            world.collide_solids(IVec2::new(-(1 << 23), -(1 << 23)), 1 << 24, 1 << 24),
            Tile::Collider
        );
        assert_eq!(
            world.collide_solids(IVec2::new(64, 64), i32::MAX, i32::MAX),
            Tile::Collider,
            "the far solid is inside the box"
        );
        assert_eq!(
            world.collide_solids(IVec2::new(16, 16), 1 << 19, 1 << 19),
            Tile::Empty
        );
        let lookups = SOLID_CELL_LOOKUPS.with(std::cell::Cell::get);
        assert!(lookups <= 6, "{lookups} cell lookups for two solids");
        // A small box still walks its own cells.
        SOLID_CELL_LOOKUPS.with(|n| n.set(0));
        assert_eq!(world.collide_solids(IVec2::new(4, 4), 8, 8), Tile::Collider);
        assert_eq!(SOLID_CELL_LOOKUPS.with(std::cell::Cell::get), 1);
    }

    #[test]
    fn an_origin_shift_is_not_read_as_a_gameplay_teleport() {
        use regia_scene::floating_origin::Rebase;
        use regia_scene::streaming::OriginShifted;
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, PlatformerPlugin));
        enable_fixed_step(&mut app, 60.0);
        {
            let mut w = app.world_mut().resource_mut::<PlatformerWorld>();
            w.add_static_tiled_layer(vec![Tile::Solid; 100], 8, 8, 100, IVec2::ZERO, 1);
        }
        let actor = app
            .world_mut()
            .spawn((
                PlatformerActor {
                    size: IVec2::new(4, 4),
                    ..PlatformerActor::default()
                },
                Transform::from_translation(regia_core::math::Vec3::new(40.0, 8.0, 0.0)),
            ))
            .id();
        for _ in 0..5 {
            app.update();
        }
        let shift = [32.0, 0.0, 0.0];
        app.world_mut()
            .get_mut::<Transform>(actor)
            .expect("transform")
            .translation
            .x -= shift[0];
        app.world_mut()
            .resource_mut::<Messages<OriginShifted>>()
            .write(OriginShifted {
                rebase: Rebase {
                    cell_delta: [1, 0, 0],
                    shift,
                },
                origin_cell: [1, 0, 0],
            });
        for _ in 0..5 {
            app.update();
        }
        let t = app
            .world()
            .get::<Transform>(actor)
            .expect("transform")
            .translation;
        assert!((t.x - 8.0).abs() < 1.0, "x = {}", t.x);
        assert!((t.y - 8.0).abs() < 1.0, "still on the floor, y = {}", t.y);
    }

    #[test]
    fn a_transform_past_i32_cannot_overflow_a_move() {
        let far = pixel(&Transform::from_translation(regia_core::math::Vec3::new(
            3e9, -3e9, 0.0,
        )));
        assert_eq!(far, IVec2::new(MAX_WORLD_PIXEL, -MAX_WORLD_PIXEL));
        let mut w = world();
        let a = w.add_actor(IVec2::ZERO, 6, 6);
        w.set_actor_position(a, far);
        assert!(w.move_h(a, 16.0));
        assert!(w.move_v(a, -16.0));
        assert_eq!(w.actor_pos(a), Some(far + IVec2::new(16, -16)));
    }

    #[test]
    fn sub_pixel_moves_carry_the_remainder() {
        let mut w = world();
        let a = w.add_actor(IVec2::new(8, 8), 6, 6);
        let mut xs = Vec::new();
        // Quarter-pixel steps (binary fractions, so rounding is exact): a pixel moves when the
        // carried remainder reaches one half.
        for _ in 0..10 {
            assert!(w.move_h(a, 0.25));
            xs.push(w.actor_pos(a).map(|p| p.x).unwrap_or_default());
            let r = w.actor_remainder(a).unwrap_or_default();
            assert!(r.x.abs() <= 0.5, "remainder {r}");
        }
        assert_eq!(xs, vec![8, 9, 9, 9, 9, 10, 10, 10, 10, 11]);
        // Position + remainder is the exact distance travelled (2.5 px).
        let r = w.actor_remainder(a).unwrap_or_default();
        assert_eq!(11.0 + r.x, 8.0 + 2.5);
        // Negative moves come back exactly.
        for _ in 0..10 {
            assert!(w.move_h(a, -0.25));
        }
        assert_eq!(w.actor_pos(a), Some(IVec2::new(8, 8)));
    }

    #[test]
    fn walls_and_floors_stop_moves_at_the_first_overlapping_pixel() {
        let mut w = world();
        let a = w.add_actor(IVec2::new(60, 8), 6, 6);
        assert!(!w.move_h(a, 20.0), "the wall at x = 72 stops it");
        assert_eq!(w.actor_pos(a), Some(IVec2::new(66, 8)));
        assert!(!w.move_v(a, -5.0), "already on the floor");
        assert_eq!(w.actor_pos(a), Some(IVec2::new(66, 8)));
        assert!(w.collide_check(a, IVec2::new(66, 7)));
    }

    #[test]
    fn one_way_tiles_block_falls_but_not_jumps_or_side_moves() {
        let mut w = world();
        // Under the platform (its tiles span y 24..32), jump up through it.
        let a = w.add_actor(IVec2::new(24, 8), 6, 6);
        assert!(w.move_v(a, 30.0), "jumping up passes through a one-way platform");
        assert_eq!(w.actor_pos(a), Some(IVec2::new(24, 38)));
        // Falling back lands on top of it (y = 32).
        assert!(!w.move_v(a, -20.0));
        assert_eq!(w.actor_pos(a), Some(IVec2::new(24, 32)));
        assert!(
            w.collide_check(a, IVec2::new(24, 31)),
            "standing on the one-way platform"
        );
        // Dropping through on request.
        w.descent(a);
        assert!(w.move_v(a, -10.0));
        assert_eq!(w.actor_pos(a), Some(IVec2::new(24, 22)));
        // Sideways through it at its own height.
        let b = w.add_actor(IVec2::new(4, 26), 4, 4);
        assert!(w.move_h(b, 30.0));
        assert_eq!(w.actor_pos(b), Some(IVec2::new(34, 26)));
        // A solid floor still blocks after all that.
        assert!(!w.move_v(a, -40.0));
        assert_eq!(w.actor_pos(a), Some(IVec2::new(24, 8)));
    }

    #[test]
    fn rider_stopped_by_a_ceiling_is_squished() {
        let mut w = world();
        let platform = w.add_solid(IVec2::new(48, 16), 16, 4);
        let rider = w.add_actor(IVec2::new(52, 20), 4, 4);
        let _ceiling = w.add_solid(IVec2::new(48, 28), 16, 4);
        w.solid_move(platform, 0.0, 8.0);
        assert!(w.squished(rider), "carried into a ceiling");
    }

    #[test]
    fn rider_left_behind_by_a_sideways_solid_is_not_squished() {
        let mut w = world();
        let platform = w.add_solid(IVec2::new(48, 16), 16, 4);
        let rider = w.add_actor(IVec2::new(52, 20), 4, 4);
        w.solid_move(platform, 3.0, 0.0);
        assert!(!w.squished(rider));
    }

    #[test]
    fn riders_follow_a_solid_on_both_axes() {
        let mut w = world();
        let platform = w.add_solid(IVec2::new(16, 40), 16, 4);
        let rider = w.add_actor(IVec2::new(20, 44), 4, 4);
        let bystander = w.add_actor(IVec2::new(40, 44), 4, 4);
        w.solid_move(platform, 3.0, 0.0);
        assert_eq!(w.actor_pos(rider), Some(IVec2::new(23, 44)));
        assert_eq!(
            w.actor_pos(bystander),
            Some(IVec2::new(40, 44)),
            "not on the platform"
        );
        w.solid_move(platform, 0.0, 5.0);
        assert_eq!(w.solid_pos(platform), Some(IVec2::new(19, 45)));
        assert_eq!(w.actor_pos(rider), Some(IVec2::new(23, 49)), "carried up");
        w.solid_move(platform, 0.0, -4.0);
        assert_eq!(w.actor_pos(rider), Some(IVec2::new(23, 45)), "carried down");
        // Sub-pixel platform speed: the rider moves exactly when the platform does.
        for _ in 0..4 {
            w.solid_move(platform, 0.25, 0.0);
        }
        assert_eq!(w.solid_pos(platform), Some(IVec2::new(20, 41)));
        assert_eq!(w.actor_pos(rider), Some(IVec2::new(24, 45)));
    }

    #[test]
    fn solids_push_actors_and_squish_them_against_walls() {
        let mut w = world();
        let door = w.add_solid(IVec2::new(40, 8), 8, 16);
        let a = w.add_actor(IVec2::new(50, 8), 6, 6);
        w.solid_move(door, 6.0, 0.0);
        assert_eq!(
            w.actor_pos(a),
            Some(IVec2::new(54, 8)),
            "pushed to the door's new right edge"
        );
        assert!(!w.squished(a));
        // The wall at x = 72: the actor (6 wide) can be pushed to x = 66 at most.
        w.solid_move(door, 20.0, 0.0);
        assert!(w.squished(a));
        assert_eq!(w.actor_pos(a), Some(IVec2::new(66, 8)));
    }

    /// Audit v033b (Med): removed actors and solids stayed as empty slots, so a game spawning and
    /// despawning one actor a step grew the storage, and every solid move's scan, forever.
    #[test]
    fn spawning_and_removing_bodies_keeps_the_storage_bounded() {
        let mut w = world();
        let mut last = None;
        for i in 0..1000 {
            let a = w.add_actor(IVec2::new(20, 8), 6, 6);
            let s = w.add_solid(IVec2::new(40, 8), 8, 8);
            if let Some(previous) = last {
                assert!(a > previous, "handles are never reused");
            }
            last = Some(a);
            if i < 999 {
                w.remove_actor(a);
                w.remove_solid(s);
            }
        }
        assert_eq!((w.actor_count(), w.solid_count()), (1, 1));
        assert_eq!(
            (w.actors.len(), w.solids.len()),
            (1, 1),
            "no slot outlives its body"
        );
    }

    #[test]
    fn removed_bodies_stop_colliding() {
        let mut w = world();
        let s = w.add_solid(IVec2::new(30, 8), 8, 8);
        let a = w.add_actor(IVec2::new(20, 8), 6, 6);
        assert!(!w.move_h(a, 10.0));
        w.remove_solid(s);
        assert!(w.move_h(a, 10.0));
        w.remove_actor(a);
        assert_eq!(w.actor_pos(a), None);
        assert_eq!((w.actor_count(), w.solid_count()), (0, 0));
    }

    #[test]
    fn ecs_actor_falls_lands_and_rides_on_the_fixed_step() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, PlatformerPlugin));
        enable_fixed_step(&mut app, 60.0);
        {
            let mut w = app.world_mut().resource_mut::<PlatformerWorld>();
            *w = world();
        }
        let actor = app
            .world_mut()
            .spawn((
                PlatformerActor {
                    size: IVec2::new(6, 6),
                    gravity: 600.0,
                    ..Default::default()
                },
                Transform::from_translation(regia_core::math::Vec3::new(20.0, 60.0, 1.0)),
            ))
            .id();
        for _ in 0..120 {
            app.update();
        }
        let t = app.world().get::<Transform>(actor).map(|t| t.translation);
        assert_eq!(
            t.map(|t| (t.x, t.y, t.z)),
            Some((20.0, 32.0, 1.0)),
            "landed on the one-way platform"
        );
        assert!(
            app.world()
                .get::<PlatformerContacts>(actor)
                .is_some_and(|c| c.grounded)
        );
        // Drop through and land on the floor.
        if let Some(mut body) = app.world_mut().get_mut::<PlatformerActor>(actor) {
            body.drop_through = true;
        }
        for _ in 0..60 {
            app.update();
        }
        let y = app.world().get::<Transform>(actor).map(|t| t.translation.y);
        assert_eq!(y, Some(8.0));
        // Despawning releases the body.
        app.world_mut().despawn(actor);
        app.update();
        assert_eq!(app.world().resource::<PlatformerWorld>().actor_count(), 0);
    }

    #[test]
    fn a_despawn_on_a_frame_without_a_fixed_step_still_releases_the_body() {
        // 1 ms frames at a 60 Hz step: most frames run no step, and removal messages are cleared
        // before the next one would read them (ADR-1660).
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, PlatformerPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(1),
            ));
        enable_fixed_step(&mut app, 60.0);
        let spawn = |app: &mut App, x: f32| {
            app.world_mut()
                .spawn((
                    PlatformerActor::default(),
                    PlatformerSolid::default(),
                    Transform::from_translation(regia_core::math::Vec3::new(x, 0.0, 0.0)),
                ))
                .id()
        };
        let (a, b) = (spawn(&mut app, 0.0), spawn(&mut app, 40.0));
        for _ in 0..40 {
            app.update();
        }
        let counts = |app: &App| {
            let w = app.world().resource::<PlatformerWorld>();
            (w.actor_count(), w.solid_count())
        };
        assert_eq!(counts(&app), (2, 2));
        app.world_mut().despawn(a);
        if let Ok(mut b) = app.world_mut().get_entity_mut(b) {
            b.remove::<(PlatformerActor, PlatformerSolid)>();
        }
        for _ in 0..40 {
            app.update();
        }
        assert_eq!(counts(&app), (0, 0));
    }

    #[test]
    fn ecs_actor_size_and_transform_edits_move_the_body() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, PlatformerPlugin));
        enable_fixed_step(&mut app, 60.0);
        {
            let mut w = app.world_mut().resource_mut::<PlatformerWorld>();
            *w = world();
        }
        let actor = app
            .world_mut()
            .spawn((
                PlatformerActor {
                    size: IVec2::new(6, 6),
                    gravity: 0.0,
                    ..Default::default()
                },
                Transform::from_translation(regia_core::math::Vec3::new(56.0, 20.0, 0.0)),
            ))
            .id();
        for _ in 0..3 {
            app.update();
        }
        if let Some(mut body) = app.world_mut().get_mut::<PlatformerActor>(actor) {
            body.size = IVec2::new(4, 4);
        }
        if let Some(mut transform) = app.world_mut().get_mut::<Transform>(actor) {
            transform.translation.x = 60.0;
            transform.translation.y = 24.0;
        }
        for _ in 0..2 {
            app.update();
        }
        let handle = *app.world().get::<Actor>(actor).expect("registered actor");
        let world = app.world().resource::<PlatformerWorld>();
        assert_eq!(world.actor_size(handle), Some((4, 4)));
        assert_eq!(world.actor_pos(handle), Some(IVec2::new(60, 24)));
    }

    #[test]
    fn stationary_solids_do_not_scan_actors() {
        let mut world = PlatformerWorld::new();
        let mut solids = Vec::new();
        for i in 0..80 {
            solids.push(world.add_solid(IVec2::new(i * 100, 0), 16, 4));
            if i < 40 {
                world.add_actor(IVec2::new(i * 100 + 2, 4), 4, 4);
            }
        }
        ACTOR_SCANS.store(0, Ordering::Relaxed);
        let before = crate::alloc_count::allocations();
        for solid in solids {
            world.solid_move(solid, 0.0, 0.0);
        }
        let allocs = crate::alloc_count::allocations() - before;
        let scans = ACTOR_SCANS.load(Ordering::Relaxed);
        assert_eq!(scans, 0, "stationary solids examined {scans} actors");
        assert_eq!(allocs, 0, "stationary solids allocated {allocs} times");
        eprintln!("stationary_solid_scans {scans} allocs {allocs}");
    }

    #[test]
    fn actor_move_ignores_distant_solids() {
        let mut crowded = PlatformerWorld::new();
        for i in 0..100 {
            crowded.add_solid(IVec2::new(10_000 + i * 20, 0), 8, 8);
        }
        crowded.add_solid(IVec2::new(16, 0), 8, 8);
        let actor = crowded.add_actor(IVec2::ZERO, 8, 8);

        let mut lone = PlatformerWorld::new();
        lone.add_solid(IVec2::new(16, 0), 8, 8);
        let lone_actor = lone.add_actor(IVec2::ZERO, 8, 8);
        let mut lone_x = 0;
        for _ in 0..20 {
            if !lone.move_h(lone_actor, 1.0) {
                break;
            }
            lone_x = lone.actor_pos(lone_actor).expect("actor").x;
        }

        SOLID_VISITS.store(0, Ordering::Relaxed);
        let mut crowded_x = 0;
        for _ in 0..20 {
            if !crowded.move_h(actor, 1.0) {
                break;
            }
            crowded_x = crowded.actor_pos(actor).expect("actor").x;
        }
        let visits = SOLID_VISITS.load(Ordering::Relaxed);
        assert_eq!(
            crowded_x, lone_x,
            "the near solid must stop the actor at the same pixel"
        );
        assert!(
            (1..40).contains(&visits),
            "solid visits {visits} (the old scan sees every far solid)"
        );
        eprintln!("platformer_solid_visits {visits}");
    }
}
