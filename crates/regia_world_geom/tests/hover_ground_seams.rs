// SPDX-License-Identifier: MPL-2.0
//! Plan row W-02 acceptance 5 and 6: the `compat::hover_ground` seams (spec 121 § 4.2, § 4.3).
//!
//! 5. A recording [`HoverGroundBody`] proves that `ground_hover_hull` and `ground_tilt_under` call only
//!    the signed-off accessors: velocity is never read (the trait has no getter for it, and the vertical
//!    speed is read only where the ledge rule grounds a hull), and `stop_horizontal` is called only by
//!    the wall rule, and only when the hull has a last position.
//! 6. A [`GroundDelta`] crater over a buried gallery moves the ground over it by the crater depth and
//!    never the tube floor (the T3 arithmetic: the delta is added on a heightfield floor only).

#![allow(clippy::unwrap_used)]

use regia_world_geom::compat::hover_ground::{
    CraterBook, GroundDelta, HULL_WALL_M, HoverGroundBody, LEDGE_M, NoGroundDelta, TiltParams,
    ground_hover_hull, ground_tilt_under, sample_move_floor,
};
use regia_world_geom::compat::mesh::synthetic_gallery;
use regia_world_geom::compat::sampler::{HEIGHT_METERS_PER_COUNT, HeightGrid, TerrainSampler};
use regia_world_geom::compat::state::TunnelsState;

/// The host's cushion-tilt tuning (3.0 m, 0.61 rad, 2.5 m).
const TILT: TiltParams = TiltParams {
    span_m: 3.0,
    max: 0.61,
    ledge_m: 2.5,
};

/// Every accessor of the trait, as the recorder logs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    OnFoot,
    Position,
    SetPosition,
    Altitude,
    SetAltitude,
    StopHorizontal,
    VerticalVel,
    SetVerticalVel,
    SetPrevTerrain,
    Yaw,
    SetGroundTilt,
}

#[derive(Debug)]
struct Recorder {
    on_foot: bool,
    position: [f32; 3],
    altitude: f32,
    vertical_vel: f32,
    prev_terrain: Option<f32>,
    yaw: f32,
    tilt: (f32, f32),
    stopped: u32,
    log: Vec<Call>,
}

impl Recorder {
    fn new(on_foot: bool, position: [f32; 3], altitude: f32) -> Self {
        Self {
            on_foot,
            position,
            altitude,
            vertical_vel: -1.5,
            prev_terrain: None,
            yaw: 0.3,
            tilt: (9.0, 9.0),
            stopped: 0,
            log: Vec::new(),
        }
    }

    fn count(&self, call: Call) -> usize {
        self.log.iter().filter(|c| **c == call).count()
    }
}

impl HoverGroundBody for Recorder {
    fn on_foot(&self) -> bool {
        self.log_ref(Call::OnFoot);
        self.on_foot
    }
    fn position(&self) -> [f32; 3] {
        self.log_ref(Call::Position);
        self.position
    }
    fn set_position(&mut self, p: [f32; 3]) {
        self.log.push(Call::SetPosition);
        self.position = p;
    }
    fn altitude(&self) -> f32 {
        self.log_ref(Call::Altitude);
        self.altitude
    }
    fn set_altitude(&mut self, a: f32) {
        self.log.push(Call::SetAltitude);
        self.altitude = a;
    }
    fn stop_horizontal(&mut self) {
        self.log.push(Call::StopHorizontal);
        self.stopped += 1;
    }
    fn vertical_vel(&self) -> f32 {
        self.log_ref(Call::VerticalVel);
        self.vertical_vel
    }
    fn set_vertical_vel(&mut self, v: f32) {
        self.log.push(Call::SetVerticalVel);
        self.vertical_vel = v;
    }
    fn set_prev_terrain(&mut self, y: Option<f32>) {
        self.log.push(Call::SetPrevTerrain);
        self.prev_terrain = y;
    }
    fn yaw(&self) -> f32 {
        self.log_ref(Call::Yaw);
        self.yaw
    }
    fn set_ground_tilt(&mut self, pitch: f32, roll: f32) {
        self.log.push(Call::SetGroundTilt);
        self.tilt = (pitch, roll);
    }
}

/// The getters take `&self`; the log of a getter call is kept through a cell so the trait stays
/// accessor-only.
impl Recorder {
    fn log_ref(&self, call: Call) {
        READS.with(|r| r.borrow_mut().push(call));
    }
}

thread_local! {
    static READS: std::cell::RefCell<Vec<Call>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs `f` and returns every accessor call it made, reads and writes, in order.
fn recorded(body: &mut Recorder, f: impl FnOnce(&mut Recorder)) -> Vec<Call> {
    READS.with(|r| r.borrow_mut().clear());
    body.log.clear();
    f(body);
    // Reads and writes interleave in time; the order of each kind is kept, and the set is what matters.
    let mut all = READS.with(|r| r.borrow().clone());
    all.extend(body.log.iter().copied());
    all
}

/// Flat ground at y = 0 west of x = 0 and a 10 m cliff east of it (a face over the wall bar).
fn cliff() -> TerrainSampler {
    let side = 32;
    let mut heights = vec![16_384_u16; side * side];
    for row in 0..side {
        for col in side / 2 + 1..side {
            heights[row * side + col] = 16_384 + 32 * 10;
        }
    }
    TerrainSampler::new(HeightGrid::square(side, heights).unwrap(), 160.0)
}

#[test]
fn hover_ground_body_touches_only_the_signed_off_fields() {
    let terrain = cliff();
    let tunnels = TunnelsState::<()>::disabled();
    let ruins = NoGroundDelta;
    let low = terrain.height_at_world(-20.0, 0.0);
    let high = terrain.height_at_world(20.0, 0.0);
    assert!(
        high - low > HULL_WALL_M + 1.0,
        "the cliff is a wall: {low} -> {high}"
    );

    // A hull cruising on flat ground: no wall, no ledge. Position, altitude, yaw and the tilt only.
    let mut hull = Recorder::new(false, [-20.0, low + 1.0, 0.0], 1.0);
    let calls = recorded(&mut hull, |b| {
        ground_hover_hull(
            b,
            Some([-21.0, low + 1.0, 0.0]),
            Some(&terrain),
            &tunnels,
            &ruins,
            &TILT,
        );
    });
    assert_eq!(hull.stopped, 0, "no wall, no stop: {calls:?}");
    for absent in [
        Call::VerticalVel,
        Call::SetVerticalVel,
        Call::SetAltitude,
        Call::SetPrevTerrain,
    ] {
        assert!(!calls.contains(&absent), "{absent:?} on a plain step: {calls:?}");
    }
    for present in [
        Call::OnFoot,
        Call::Position,
        Call::Altitude,
        Call::SetPosition,
        Call::Yaw,
        Call::SetGroundTilt,
    ] {
        assert!(calls.contains(&present), "{present:?} missing: {calls:?}");
    }
    assert_eq!(hull.count(Call::SetGroundTilt), 1);

    // The wall rule: the hull moved into the cliff face goes back to its last position and stops.
    let mut hull = Recorder::new(false, [20.0, low + 1.0, 0.0], 1.0);
    let calls = recorded(&mut hull, |b| {
        ground_hover_hull(
            b,
            Some([-20.0, low + 1.0, 0.0]),
            Some(&terrain),
            &tunnels,
            &ruins,
            &TILT,
        );
    });
    assert_eq!(hull.stopped, 1, "the wall rule stops the hull once: {calls:?}");
    assert_eq!(hull.position[0], -20.0, "back to its last position");
    assert!(
        !calls.contains(&Call::VerticalVel),
        "no ledge, no vertical read: {calls:?}"
    );

    // The wall rule without a last position (the hull's first tick) waits: no stop.
    let mut hull = Recorder::new(false, [20.0, low + 1.0, 0.0], 1.0);
    ground_hover_hull(&mut hull, None, Some(&terrain), &tunnels, &ruins, &TILT);
    assert_eq!(hull.stopped, 0, "first tick: the wall rule waits");

    // On foot: no wall rule and no tilt, so neither the stop nor the heading nor the tilt is touched.
    let mut walker = Recorder::new(true, [20.0, low + 1.0, 0.0], 0.0);
    let calls = recorded(&mut walker, |b| {
        ground_hover_hull(
            b,
            Some([-20.0, low + 1.0, 0.0]),
            Some(&terrain),
            &tunnels,
            &ruins,
            &TILT,
        );
    });
    assert_eq!(walker.stopped, 0, "{calls:?}");
    assert!(
        !calls.contains(&Call::Yaw) && !calls.contains(&Call::SetGroundTilt),
        "{calls:?}"
    );

    // The ledge rule that grounds a hull (the floor rose by more than a ledge, under the wall bar):
    // the one place the vertical speed is read, and it is only raised to zero.
    let step = 1.0 + LEDGE_M;
    let side = 32;
    let mut heights = vec![16_384_u16; side * side];
    for row in 0..side {
        for col in side / 2 + 1..side {
            heights[row * side + col] = 16_384 + (step / HEIGHT_METERS_PER_COUNT) as u16;
        }
    }
    let ledge = TerrainSampler::new(HeightGrid::square(side, heights).unwrap(), 160.0);
    let base = ledge.height_at_world(-20.0, 0.0);
    let mut hull = Recorder::new(false, [20.0, base + 0.5, 0.0], 0.5);
    let calls = recorded(&mut hull, |b| {
        ground_hover_hull(
            b,
            Some([-20.0, base + 0.5, 0.0]),
            Some(&ledge),
            &tunnels,
            &ruins,
            &TILT,
        );
    });
    assert_eq!(hull.stopped, 0, "{calls:?}");
    assert_eq!(
        calls.iter().filter(|c| **c == Call::VerticalVel).count(),
        1,
        "{calls:?}"
    );
    assert_eq!(
        hull.vertical_vel, 0.0,
        "a falling spring is stopped on the risen floor"
    );
    assert!(
        calls.contains(&Call::SetAltitude) && calls.contains(&Call::SetPrevTerrain),
        "{calls:?}"
    );

    // `ground_tilt_under` reads the position and the heading only.
    let mut hull = Recorder::new(false, [-20.0, low + 1.0, 0.0], 1.0);
    let calls = recorded(&mut hull, |b| {
        let _ = ground_tilt_under(&*b, low, Some(&terrain), &tunnels, &ruins, &TILT);
    });
    let mut kinds = calls.clone();
    kinds.sort_by_key(|c| *c as u8);
    kinds.dedup();
    assert_eq!(kinds, vec![Call::Position, Call::Yaw], "{calls:?}");
}

/// Flat ground at y = 0.
fn flat() -> TerrainSampler {
    let raw = (8.0 / HEIGHT_METERS_PER_COUNT) as u16;
    TerrainSampler::new(HeightGrid::square(4, vec![raw; 16]).unwrap(), 80.0)
}

#[test]
fn a_crater_moves_the_ground_but_never_a_tunnel_floor_stage_a() {
    let terrain = flat();
    // A buried gallery: floor 12 m down, lid 6 m down, the ground over it at 0.
    let tunnels = TunnelsState::<()>::with_mesh(true, synthetic_gallery(10.0, 10.0, -12.0, -6.0, false));
    let mut craters = CraterBook::default();
    craters.crater(0.0, 0.0);
    let depth = craters.craters[0].depth;
    assert!(craters.height_delta(0.0, 0.0) < 0.0);
    for (x, z) in [(0.0f32, 0.0f32), (1.5, -1.0), (-2.0, 2.0)] {
        // In the tube: the crater does not reach the floor (the delta is never added to a mesh floor).
        let bare = sample_move_floor(Some(&terrain), &tunnels, &NoGroundDelta, x, z, -12.0);
        let cratered = sample_move_floor(Some(&terrain), &tunnels, &craters, x, z, -12.0);
        assert!((bare + 12.0).abs() < 1e-4, "the tube floor: {bare}");
        assert_eq!(
            cratered.to_bits(),
            bare.to_bits(),
            "a crater moved the tube floor at ({x}, {z})"
        );
        // On the ground over it: lowered by the crater depth, as the same f32 add.
        let ground = sample_move_floor(Some(&terrain), &tunnels, &NoGroundDelta, x, z, 0.0);
        let dipped = sample_move_floor(Some(&terrain), &tunnels, &craters, x, z, 0.0);
        assert!(ground.abs() < 1e-4, "the ground over the tube: {ground}");
        assert_eq!(
            dipped.to_bits(),
            (ground + -depth).to_bits(),
            "the ground in the crater at ({x}, {z})"
        );
    }
    // Outside the crater nothing moves.
    let far = sample_move_floor(Some(&terrain), &tunnels, &craters, 4.5, 0.0, 0.0);
    assert_eq!(
        far.to_bits(),
        sample_move_floor(Some(&terrain), &tunnels, &NoGroundDelta, 4.5, 0.0, 0.0).to_bits()
    );
    // Disarmed tunnels: the crater is on the heightfield as before.
    let off = TunnelsState::<()>::disabled();
    let dipped = sample_move_floor(Some(&terrain), &off, &craters, 0.0, 0.0, 0.0);
    assert!((dipped + depth).abs() < 1e-4, "{dipped}");
}
