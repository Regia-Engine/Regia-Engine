// SPDX-License-Identifier: MPL-2.0
//! The lockstep soak (spec 120 § 7, § 15.1 `lockstep-mix`; ADR-8146): a `LockstepHost` on a real
//! `NetServer` and `LockstepSeat`s on real `NetClient`s, over the impaired in-process network, in
//! virtual time.
//!
//! - [`LockGame`]: the game vocabulary (its only message is a lockstep payload).
//! - [`LockSim`]: a small deterministic hover-steering simulation. Every float operation that is
//!   not a basic IEEE operation goes through `regia_detsim::math`, min/max/clamp through
//!   `regia_detsim::float`, and the hash folds canonical bits, so the same inputs give the same
//!   hash on x86-64 and aarch64 (`tests/lockstep_golden.rs` pins it).
//! - [`InputSource`]: a seeded input script per seat ([`InputMix`] says how often inputs change).
//! - [`run_lockstep`]: one run; [`LockstepRecord`] holds what the bars read.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;

use regia_detsim::float::{canonical, det_clamp, det_max};
use regia_detsim::math;
use regia_net::desync::{BundleFrame, BundleSnapshot, DesyncHash, DesyncReplay, DesyncReport, GameIdentity};
use regia_net::lockstep::net::{LockstepCarrier, host_receive, seat_receive, send_host, send_seat};
use regia_net::lockstep::replay::{LsReplayHeader, LsReplaySettings, LsReplayWriter, ReplayAbortInfo};
use regia_net::lockstep::{
    DesyncVerdict, HostEvent, HostRole, InputRules, LockstepConfig, LockstepFrame, LockstepHost,
    LockstepMode, LockstepSeat, LockstepStatus, MembershipChange, MembershipKind, RollbackGame,
    RollbackRunner, SeatEvent, SeatId, SeatInput, Voter,
};
use regia_net::{
    ClientConfig, ClientEvent, NetClient, NetCodec, NetGame, NetServer, PeerId, ServerConfig, ServerEvent,
    WireError, WireReader, WireWriter,
};
use serde::{Deserialize, Serialize};

use crate::impair::{IP_UDP_OVERHEAD, ImpairedNet, ImpairedSocket, SoakRng};
use crate::profiles::Profile;

/// The lockstep sample game: its only message is a lockstep payload.
#[derive(Debug, Clone, Copy)]
pub struct LockGame;

impl NetGame for LockGame {
    type Message = Vec<u8>;
    type Input = LockInput;
    /// The seat the client asks for.
    type JoinData = u32;
    const GAME_ID: &'static str = "regia.net_soak.lockstep";
    const PROTOCOL_VERSION: u32 = 1;
}

impl LockstepCarrier for LockGame {
    fn wrap(payload: Vec<u8>) -> Vec<u8> {
        payload
    }
    fn unwrap(message: &Vec<u8>) -> Option<&[u8]> {
        Some(message)
    }
}

/// Largest command blob of one input, bytes.
pub const MAX_COMMAND: usize = 255;

/// One tick of a seat's input: 13 bytes plus an optional command, all integers on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LockInput {
    /// Throttle, -127..=127.
    pub throttle: i8,
    /// Strafe, -127..=127.
    pub strafe: i8,
    /// Aim heading in 1/65536 turns.
    pub aim: u16,
    /// Aim pitch.
    pub pitch: i16,
    /// Button bits; bit 0 fires.
    pub buttons: u16,
    /// Stance.
    pub stance: u8,
    /// Selected weapon.
    pub weapon: u8,
    /// Designated target (a seat plus one; 0: none).
    pub designate: u16,
    /// A command blob (0..=[`MAX_COMMAND`] bytes).
    pub command: Vec<u8>,
}

impl NetCodec for LockInput {
    fn encode(&self, w: &mut WireWriter) {
        w.u8(self.throttle as u8);
        w.u8(self.strafe as u8);
        w.u16(self.aim);
        w.u16(self.pitch as u16);
        w.u16(self.buttons);
        w.u8(self.stance);
        w.u8(self.weapon);
        w.u16(self.designate);
        let n = self.command.len().min(MAX_COMMAND);
        w.u8(n as u8);
        w.raw(self.command.get(..n).unwrap_or_default());
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        let throttle = r.u8()? as i8;
        let strafe = r.u8()? as i8;
        let aim = r.u16()?;
        let pitch = r.u16()? as i16;
        let buttons = r.u16()?;
        let stance = r.u8()?;
        let weapon = r.u8()?;
        let designate = r.u16()?;
        let n = usize::from(r.u8()?);
        let command = r.take(n)?.to_vec();
        Ok(Self {
            throttle,
            strafe,
            aim,
            pitch,
            buttons,
            stance,
            weapon,
            designate,
            command,
        })
    }
}

/// Which generator [`InputMix`] runs.
///
/// Old records omit the field and decode as [`InputMixKind::Rates`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InputMixKind {
    /// The per-mille rates on [`InputMix`].
    #[default]
    Rates,
    /// A held heading, fire pulses and target lead (`lockstep-mix+bots`, ADR-8409).
    WanderFire,
}

/// How often a seat's input changes, per thousand ticks, or the wander-fire bot mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputMix {
    /// The aim moves (one or two bytes change).
    pub aim_per_mille: u32,
    /// The move axes change.
    pub move_per_mille: u32,
    /// A button, the stance or the weapon changes.
    pub button_per_mille: u32,
    /// The tick carries a command blob.
    pub command_per_mille: u32,
    /// Bytes of a command blob.
    pub command_bytes: usize,
    /// `rates` uses the fields above. `wander_fire` ignores them (ADR-8409).
    #[serde(default)]
    pub kind: InputMixKind,
}

impl InputMix {
    /// An order-driven match: seats mostly hold their input and issue commands (the engine's
    /// stand-in for Direct Order 27's recorded mix, which is not in this repository).
    pub const ORDERS: Self = Self {
        aim_per_mille: 80,
        move_per_mille: 30,
        button_per_mille: 20,
        command_per_mille: 4,
        command_bytes: 24,
        kind: InputMixKind::Rates,
    };
    /// An action match: every seat moves its aim on half of its ticks, so on any tick half of
    /// the seats change their input (the second soak mix, `lockstep-mix+action`; ADR-8209).
    pub const ACTION: Self = Self {
        aim_per_mille: 500,
        move_per_mille: 50,
        button_per_mille: 30,
        command_per_mille: 2,
        command_bytes: 24,
        kind: InputMixKind::Rates,
    };
    /// Seeded wander, fire pulses and target lead (the third soak mix, `lockstep-mix+bots`;
    /// ADR-8409). The per-mille fields are unused.
    pub const WANDER_FIRE: Self = Self {
        aim_per_mille: 0,
        move_per_mille: 0,
        button_per_mille: 0,
        command_per_mille: 0,
        command_bytes: 0,
        kind: InputMixKind::WanderFire,
    };
}

/// Frames a wander or lead heading is held, inclusive. Defaults match the recorded bot driver.
pub const WANDER_HOLD_MIN: u32 = 60;
/// `WANDER_HOLD_MIN + this` is the longest hold, so the range is 60..=180.
pub const WANDER_HOLD_EXTRA: u32 = 120;
/// Frames between fire pulses, inclusive lower bound.
pub const FIRE_PERIOD_MIN: u32 = 120;
/// `FIRE_PERIOD_MIN + this` is the longest gap, so the range is 120..=300.
pub const FIRE_PERIOD_EXTRA: u32 = 180;
/// Frames the trigger stays down once a pulse starts.
pub const FIRE_PULSE_FRAMES: u32 = 4;
/// Lead shot speed, metres per second. A heading is aimed at where the target will be.
pub const LEAD_SPEED_MPS: f32 = 150.0;
/// A hull farther than this is not a target, metres.
pub const ACQUIRE_RANGE_M: f32 = 200.0;

/// A seat's seeded input script: integer arithmetic only, so it is the same on every host.
#[derive(Debug, Clone)]
pub struct InputSource {
    rng: SoakRng,
    mix: InputMix,
    current: LockInput,
    seat: u16,
    hold_left: u32,
    aim_held: u16,
    fire_in: u32,
    pulse_left: u32,
    /// Whether the current hold may pull the trigger (no target, or one inside the cone).
    fire_ok: bool,
}

impl InputSource {
    /// The script of `seat` under `seed`.
    pub fn new(seed: u64, seat: u16, mix: InputMix) -> Self {
        Self {
            rng: SoakRng::derive(seed, 0x10c5_0000 + u64::from(seat)),
            mix,
            current: LockInput {
                aim: seat.wrapping_mul(1543),
                throttle: 127,
                ..LockInput::default()
            },
            seat,
            hold_left: 0,
            aim_held: 0,
            fire_in: 0,
            pulse_left: 0,
            fire_ok: true,
        }
    }

    fn roll(&mut self, per_mille: u32) -> bool {
        self.rng.up_to(999) < u64::from(per_mille)
    }

    /// The input for the next capture tick, from the seed alone.
    pub fn next_input(&mut self) -> LockInput {
        self.next_input_with(&[])
    }

    /// The input for the next capture tick. `hulls` is `(seat, x, y, heading, speed)` in seat
    /// order, the agreed simulation before this input is applied. Rate mixes ignore it. The
    /// wander-fire mix uses it to lead a moving hull.
    pub fn next_input_with(&mut self, hulls: &[(u16, f32, f32, f32, f32)]) -> LockInput {
        if self.mix.kind == InputMixKind::WanderFire {
            return self.wander_fire(hulls);
        }
        self.rate_input()
    }

    fn rate_input(&mut self) -> LockInput {
        self.current.command.clear();
        if self.roll(self.mix.aim_per_mille) {
            let delta = self.rng.up_to(160) as u16;
            self.current.aim = self.current.aim.wrapping_add(delta).wrapping_sub(80);
        }
        if self.roll(self.mix.move_per_mille) {
            self.current.throttle = [-127i8, 0, 127, 127][self.rng.up_to(3) as usize];
            self.current.strafe = [-127i8, 0, 0, 127][self.rng.up_to(3) as usize];
        }
        if self.roll(self.mix.button_per_mille) {
            match self.rng.up_to(3) {
                0 => self.current.buttons ^= 1,
                1 => self.current.buttons ^= 1 << (1 + self.rng.up_to(7)),
                2 => self.current.stance = self.rng.up_to(3) as u8,
                _ => self.current.weapon = self.rng.up_to(5) as u8,
            }
        }
        if self.roll(self.mix.command_per_mille) {
            self.current.command = (0..self.mix.command_bytes.min(MAX_COMMAND))
                .map(|_| self.rng.next_u64() as u8)
                .collect();
        }
        self.current.clone()
    }

    /// A held heading, a throttle of 127 and a short fire pulse.
    ///
    /// The heading is rolled once per hold (60..=180 frames) and then kept, so the aim word on
    /// the wire stays still. With no hull in range the heading wanders. With one, the hold aims
    /// at where that hull will be if a shot at [`LEAD_SPEED_MPS`] is fired now, using the agreed
    /// heading and speed (every participant already shares [`LockSim`]). The trigger pulses for
    /// [`FIRE_PULSE_FRAMES`] every 120..=300 frames. A wander always may fire. A lead fires only
    /// when the target is inside the weapon range and the hull is already inside the weapon cone;
    /// the timer still advances.
    fn wander_fire(&mut self, hulls: &[(u16, f32, f32, f32, f32)]) -> LockInput {
        if self.hold_left == 0 {
            let extra = u32::try_from(self.rng.up_to(u64::from(WANDER_HOLD_EXTRA))).unwrap_or(0);
            self.hold_left = WANDER_HOLD_MIN + extra;
            let (aim, fire_ok) = self.choose_aim(hulls);
            self.aim_held = aim;
            self.fire_ok = fire_ok;
        }
        self.hold_left = self.hold_left.saturating_sub(1);
        if self.fire_in == 0 && self.pulse_left == 0 {
            let extra = u32::try_from(self.rng.up_to(u64::from(FIRE_PERIOD_EXTRA))).unwrap_or(0);
            self.fire_in = FIRE_PERIOD_MIN + extra;
            if self.fire_ok {
                self.pulse_left = FIRE_PULSE_FRAMES;
            }
        }
        if self.fire_in > 0 {
            self.fire_in -= 1;
        }
        let firing = self.pulse_left > 0;
        if self.pulse_left > 0 {
            self.pulse_left -= 1;
        }
        LockInput {
            throttle: 127,
            strafe: 0,
            aim: self.aim_held,
            buttons: u16::from(firing),
            ..LockInput::default()
        }
    }

    /// The aim for a new hold, and whether this hold may pull the trigger.
    fn choose_aim(&mut self, hulls: &[(u16, f32, f32, f32, f32)]) -> (u16, bool) {
        let Some(mine) = hulls.iter().find(|hull| hull.0 == self.seat) else {
            return (self.wander_aim(), true);
        };
        let (_, mx, my, my_heading, _) = *mine;
        let mut best: Option<(f32, f32, f32, f32, f32)> = None;
        for &(seat, x, y, heading, speed) in hulls {
            if seat == self.seat {
                continue;
            }
            let (dx, dy) = (x - mx, y - my);
            let d2 = dx * dx + dy * dy;
            if d2 < 1.0e-6 || d2 > ACQUIRE_RANGE_M * ACQUIRE_RANGE_M {
                continue;
            }
            if best.is_none_or(|(bd, _, _, _, _)| d2 < bd) {
                best = Some((d2, x, y, heading, speed));
            }
        }
        let Some((d2, tx, ty, heading, speed)) = best else {
            return (self.wander_aim(), true);
        };
        let d = math::sqrt(d2);
        let flight = d / LEAD_SPEED_MPS;
        let vx = math::cos(heading) * speed;
        let vy = math::sin(heading) * speed;
        let lx = (tx - mx) + vx * flight;
        let ly = (ty - my) + vy * flight;
        let bearing = math::atan2(ly, lx);
        let off = wrap_angle(bearing - my_heading);
        let fire_ok = d <= WEAPON_RANGE && off.abs() < WEAPON_CONE;
        (radians_to_aim(bearing), fire_ok)
    }

    /// A heading drawn from the seed: tenths of a degree, then the aim word.
    fn wander_aim(&mut self) -> u16 {
        let tenths = self.rng.up_to(3_599) as f32;
        radians_to_aim(tenths * (core::f32::consts::TAU / 3_600.0))
    }
}

/// An angle in radians as an aim word: `0..tau` maps onto `0..65536`, clamped to a `u16`.
fn radians_to_aim(angle: f32) -> u16 {
    let wrapped = wrap_angle(angle);
    let shifted = if wrapped < 0.0 {
        wrapped + core::f32::consts::TAU
    } else {
        wrapped
    };
    let scaled = det_clamp(shifted * (65_536.0 / core::f32::consts::TAU), 0.0, 65_535.0);
    u16::try_from(scaled as u32).unwrap_or(0)
}

const TURN_PER_UNIT: f32 = core::f32::consts::TAU / 65_536.0;
const TURN_RATE: f32 = 2.5;
const MAX_SPEED: f32 = 12.0;
const STRAFE_SPEED: f32 = 4.0;
const ACCEL: f32 = 3.0;
const ARENA: f32 = 400.0;
const HULL_RADIUS: f32 = 3.0;
const WEAPON_RANGE: f32 = 80.0;
const WEAPON_CONE: f32 = 0.25;
const WEAPON_COOLDOWN: u32 = 30;
const WEAPON_DAMAGE: f32 = 40.0;

/// One hover hull.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hull {
    /// Position, metres.
    pub x: f32,
    /// Position, metres.
    pub y: f32,
    /// Heading, radians in `(-pi, pi]`.
    pub heading: f32,
    /// Forward speed, metres per second.
    pub speed: f32,
    /// Health; a hull at zero respawns.
    pub health: i32,
    /// Ticks until the weapon fires again.
    pub cooldown: u32,
    aim: u16,
    throttle: i8,
    strafe: i8,
    fire: bool,
}

impl Hull {
    fn spawn(seat: u16) -> Self {
        // A ring of spawn points: integers to floats exactly, then libm.
        let angle = f32::from(seat % 64) * (core::f32::consts::TAU / 64.0);
        let ring = 60.0 + f32::from(seat / 64) * 30.0;
        Self {
            x: math::cos(angle) * ring,
            y: math::sin(angle) * ring,
            heading: angle,
            speed: 0.0,
            health: 100,
            cooldown: 0,
            aim: 0,
            throttle: 0,
            strafe: 0,
            fire: false,
        }
    }
}

/// The deterministic simulation every participant steps from frames. It serializes (JSON, every
/// float exact) for a catch-up's state transfer (spec 120 § 7.4).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LockSim {
    /// The next tick to step.
    pub tick: u64,
    /// Hulls by seat.
    pub hulls: BTreeMap<u16, Hull>,
    /// Shots that hit, so far.
    pub hits: u64,
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a over the eight little-endian bytes of `word`.
fn fnv_word(mut h: u64, word: u64) -> u64 {
    for byte in word.to_le_bytes() {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn wrap_angle(a: f32) -> f32 {
    math::atan2(math::sin(a), math::cos(a))
}

impl LockSim {
    /// Steps one frame.
    pub fn step(&mut self, frame: &LockstepFrame<LockInput>) {
        for change in &frame.membership {
            match change.kind {
                MembershipKind::Join => {
                    self.hulls
                        .entry(change.seat.0)
                        .or_insert_with(|| Hull::spawn(change.seat.0));
                }
                MembershipKind::Leave => {
                    self.hulls.remove(&change.seat.0);
                }
            }
        }
        let dt = regia_detsim::tick::tick_dt();
        let blend = 1.0 - math::exp(-ACCEL * dt);
        for (seat, input) in &frame.inputs {
            let Some(h) = self.hulls.get_mut(&seat.0) else {
                continue;
            };
            match input {
                // An input the match's missing-input policy holds steps like a sent one.
                SeatInput::Present(i) | SeatInput::Held(i) => {
                    h.aim = i.aim;
                    h.throttle = i.throttle;
                    h.strafe = i.strafe;
                    h.fire = i.buttons & 1 != 0;
                }
                // A missing input holds the analog values and releases the trigger.
                SeatInput::Absent => h.fire = false,
            }
            let want = f32::from(h.aim) * TURN_PER_UNIT;
            let turn = det_clamp(wrap_angle(want - h.heading), -TURN_RATE * dt, TURN_RATE * dt);
            h.heading = wrap_angle(h.heading + turn);
            h.speed += (f32::from(h.throttle) * (MAX_SPEED / 127.0) - h.speed) * blend;
            let (s, c) = (math::sin(h.heading), math::cos(h.heading));
            let side = f32::from(h.strafe) * (STRAFE_SPEED / 127.0);
            h.x = det_clamp(h.x + (c * h.speed - s * side) * dt, -ARENA, ARENA);
            h.y = det_clamp(h.y + (s * h.speed + c * side) * dt, -ARENA, ARENA);
        }
        let ids: Vec<u16> = self.hulls.keys().copied().collect();
        // Hulls push each other apart, pairs in seat order.
        for (i, a) in ids.iter().enumerate() {
            for b in ids.get(i + 1..).unwrap_or_default() {
                let (Some(ha), Some(hb)) = (self.hulls.get(a), self.hulls.get(b)) else {
                    continue;
                };
                let (dx, dy) = (hb.x - ha.x, hb.y - ha.y);
                let d2 = dx * dx + dy * dy;
                if d2 >= (2.0 * HULL_RADIUS) * (2.0 * HULL_RADIUS) {
                    continue;
                }
                let d = math::sqrt(det_max(d2, 1.0e-6));
                let push = (2.0 * HULL_RADIUS - d) * 0.5 / d;
                let (px, py) = (dx * push, dy * push);
                if let Some(ha) = self.hulls.get_mut(a) {
                    ha.x -= px;
                    ha.y -= py;
                }
                if let Some(hb) = self.hulls.get_mut(b) {
                    hb.x += px;
                    hb.y += py;
                }
            }
        }
        // Weapons, shooters in seat order: the nearest hull inside the cone takes the hit.
        for a in &ids {
            let Some(shooter) = self.hulls.get(a).cloned() else {
                continue;
            };
            if shooter.cooldown > 0 || !shooter.fire {
                if let Some(h) = self.hulls.get_mut(a) {
                    h.cooldown = h.cooldown.saturating_sub(1);
                }
                continue;
            }
            let mut best: Option<(f32, u16)> = None;
            for b in ids.iter().filter(|b| *b != a) {
                let Some(target) = self.hulls.get(b) else {
                    continue;
                };
                let (dx, dy) = (target.x - shooter.x, target.y - shooter.y);
                let d = math::sqrt(dx * dx + dy * dy);
                let off = wrap_angle(math::atan2(dy, dx) - shooter.heading);
                if d < WEAPON_RANGE && off.abs() < WEAPON_CONE && best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, *b));
                }
            }
            if let Some(h) = self.hulls.get_mut(a) {
                h.cooldown = WEAPON_COOLDOWN;
            }
            if let Some((d, b)) = best
                && let Some(target) = self.hulls.get_mut(&b)
            {
                let damage = (WEAPON_DAMAGE * math::powf(1.0 - d / WEAPON_RANGE, 1.5)) as i32 + 1;
                target.health -= damage;
                self.hits += 1;
                if target.health <= 0 {
                    *target = Hull::spawn(b);
                }
            }
        }
        self.tick = frame.tick + 1;
    }

    /// The state hash: FNV-1a over the tick, the hit count and every hull's canonical bits
    /// (one NaN pattern, one zero), in seat order.
    pub fn hash(&self) -> u64 {
        self.fold_hulls(fnv_word(fnv_word(FNV_OFFSET, self.tick), self.hits))
    }

    /// Folds every hull into `h`, in seat order.
    fn fold_hulls(&self, mut h: u64) -> u64 {
        for (seat, hull) in &self.hulls {
            h = fnv_word(h, u64::from(*seat));
            h = fnv_word(h, u64::from(canonical(hull.x).to_bits()));
            h = fnv_word(h, u64::from(canonical(hull.y).to_bits()));
            h = fnv_word(h, u64::from(canonical(hull.heading).to_bits()));
            h = fnv_word(h, u64::from(canonical(hull.speed).to_bits()));
            h = fnv_word(h, hull.health as u32 as u64);
            h = fnv_word(h, u64::from(hull.cooldown));
        }
        h
    }

    /// The per-subsystem words of [`Self::hash`]'s fields, FNV-1a each: `roster` (the tick and
    /// the seats), `motion` (position, heading, speed) and `combat` (hits, health, cooldown).
    fn subsystem_words(&self) -> [u64; 3] {
        fn fold(h: &mut u64, word: u64) {
            for byte in word.to_le_bytes() {
                *h ^= u64::from(byte);
                *h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        let mut words = [0xcbf2_9ce4_8422_2325u64; 3];
        let [roster, motion, combat] = &mut words;
        fold(roster, self.tick);
        fold(combat, self.hits);
        for (seat, hull) in &self.hulls {
            fold(roster, u64::from(*seat));
            fold(motion, u64::from(canonical(hull.x).to_bits()));
            fold(motion, u64::from(canonical(hull.y).to_bits()));
            fold(motion, u64::from(canonical(hull.heading).to_bits()));
            fold(motion, u64::from(canonical(hull.speed).to_bits()));
            fold(combat, hull.health as u32 as u64);
            fold(combat, u64::from(hull.cooldown));
        }
        words
    }

    /// Agreed hull motion in seat order: `(seat, x, y, heading, speed)`.
    ///
    /// The wander-fire mix leads from this. Heading 0 is +X. The speed is along the heading.
    pub fn motion(&self) -> Vec<(u16, f32, f32, f32, f32)> {
        self.hulls
            .iter()
            .map(|(seat, hull)| (*seat, hull.x, hull.y, hull.heading, hull.speed))
            .collect()
    }

    /// The hash of the hulls alone, without the tick and the hit count: it changes only when a
    /// hull does. ([`Self::hash`] folds the tick, so it differs between any two ticks whatever
    /// the simulation did.)
    pub fn body_hash(&self) -> u64 {
        self.fold_hulls(FNV_OFFSET)
    }

    /// The test-only fault hook: moves one hull half a metre, as a simulation bug or a flipped
    /// bit would. Compiled exactly when the `desync-inject` feature is on (`crate::DESYNC_INJECT`).
    #[cfg(feature = "desync-inject")]
    pub fn perturb(&mut self) {
        if let Some(hull) = self.hulls.values_mut().next() {
            hull.x += 0.5;
        }
    }
}

/// The dedicated lockstep server's observer steps the same simulation (ADR-8558).
impl regia_net::lockstep::server::LockstepObserver<LockInput> for LockSim {
    fn step(&mut self, frame: &LockstepFrame<LockInput>) {
        LockSim::step(self, frame);
    }

    fn state_hash(&self, _tick: u64) -> u64 {
        self.hash()
    }

    /// The soak's catch-up bytes (JSON) load (ADR-8855).
    fn loads_state(&self) -> bool {
        true
    }

    fn load_state(&mut self, _tick: u64, state: &[u8]) -> bool {
        match serde_json::from_slice::<LockSim>(state) {
            Ok(sim) => {
                *self = sim;
                true
            }
            Err(_) => false,
        }
    }
}

/// The subsystems [`LockSim`] names for the desync probe (spec 120 § 7.6).
pub const LOCK_SIM_SUBSYSTEMS: [&str; 3] = ["roster", "motion", "combat"];

impl DesyncHash for LockSim {
    /// [`LockSim::hash`]: the golden-pinned world hash.
    fn world_hash(&self, _tick: u64) -> u64 {
        self.hash()
    }

    fn subsystem_hashes(&self, _tick: u64, out: &mut Vec<(&'static str, u64)>) {
        out.clear();
        out.extend(LOCK_SIM_SUBSYSTEMS.into_iter().zip(self.subsystem_words()));
    }
}

impl RollbackGame<LockInput> for LockSim {
    fn step(&mut self, frame: &LockstepFrame<LockInput>) {
        LockSim::step(self, frame);
    }
    fn hash(&self) -> u64 {
        LockSim::hash(self)
    }
}

/// The sample game's input rules (spec 120 § 6.3, § 7.7): a command blob is the one-shot press;
/// the axes, the aim and the held buttons are analog. Set on every participant of a rollback run.
pub fn lock_input_rules() -> InputRules<LockInput> {
    InputRules {
        clear_presses: |input| LockInput {
            command: Vec::new(),
            ..input.clone()
        },
        merge_late: |late, next| {
            if next.command.is_empty() {
                next.command.clone_from(&late.command);
            }
        },
    }
}

/// Bytes of one hull in [`LockSim::save_state`]: seat, four floats' raw bits, health, cooldown,
/// aim, throttle, strafe, fire.
const HULL_STATE_BYTES: usize = 2 + 4 * 4 + 4 + 4 + 2 + 1 + 1 + 1;

impl LockSim {
    /// Appends the state's encoding (a `.lsreplay` Full; ADR-8513): the tick, the hit count and
    /// every hull in seat order, floats as their raw bits, little endian.
    pub fn save_state(&self, out: &mut Vec<u8>) {
        out.reserve(20 + self.hulls.len() * HULL_STATE_BYTES);
        out.extend_from_slice(&self.tick.to_le_bytes());
        out.extend_from_slice(&self.hits.to_le_bytes());
        out.extend_from_slice(&(self.hulls.len() as u32).to_le_bytes());
        for (seat, h) in &self.hulls {
            out.extend_from_slice(&seat.to_le_bytes());
            for f in [h.x, h.y, h.heading, h.speed] {
                out.extend_from_slice(&f.to_bits().to_le_bytes());
            }
            out.extend_from_slice(&h.health.to_le_bytes());
            out.extend_from_slice(&h.cooldown.to_le_bytes());
            out.extend_from_slice(&h.aim.to_le_bytes());
            out.push(h.throttle as u8);
            out.push(h.strafe as u8);
            out.push(u8::from(h.fire));
        }
    }

    /// The state [`Self::save_state`] wrote; every length is checked against the bytes.
    pub fn load_state(bytes: &[u8]) -> Result<Self, String> {
        let (head, mut rest) = bytes
            .split_first_chunk::<20>()
            .ok_or_else(|| format!("a LockSim state is at least 20 bytes, not {}", bytes.len()))?;
        let word = |at: usize| u64::from_le_bytes(head[at..at + 8].try_into().unwrap_or([0; 8]));
        let count = u32::from_le_bytes([head[16], head[17], head[18], head[19]]) as usize;
        if rest.len() != count.saturating_mul(HULL_STATE_BYTES) {
            return Err(format!(
                "{count} hulls need {} bytes, the state has {}",
                count * HULL_STATE_BYTES,
                rest.len()
            ));
        }
        let mut sim = Self {
            tick: word(0),
            hits: word(8),
            hulls: BTreeMap::new(),
        };
        while let Some((h, tail)) = rest.split_first_chunk::<HULL_STATE_BYTES>() {
            rest = tail;
            let f = |at: usize| f32::from_bits(u32::from_le_bytes([h[at], h[at + 1], h[at + 2], h[at + 3]]));
            let seat = u16::from_le_bytes([h[0], h[1]]);
            let hull = Hull {
                x: f(2),
                y: f(6),
                heading: f(10),
                speed: f(14),
                health: i32::from_le_bytes([h[18], h[19], h[20], h[21]]),
                cooldown: u32::from_le_bytes([h[22], h[23], h[24], h[25]]),
                aim: u16::from_le_bytes([h[26], h[27]]),
                throttle: h[28] as i8,
                strafe: h[29] as i8,
                fire: match h[30] {
                    0 => false,
                    1 => true,
                    v => return Err(format!("hull {seat}: fire flag {v}")),
                },
            };
            if sim.hulls.insert(seat, hull).is_some() {
                return Err(format!("hull {seat} is listed twice"));
            }
        }
        Ok(sim)
    }
}

/// The `DesyncReplay` hook of [`LockGame`]: `regia net replay-audit` and `desync-replay` step a
/// [`LockSim`] with it (ADR-8513).
#[derive(Debug, Clone, Default)]
pub struct LockReplay {
    /// The simulation.
    pub sim: LockSim,
}

impl DesyncHash for LockReplay {
    fn world_hash(&self, tick: u64) -> u64 {
        self.sim.world_hash(tick)
    }
    fn subsystem_hashes(&self, tick: u64, out: &mut Vec<(&'static str, u64)>) {
        self.sim.subsystem_hashes(tick, out);
    }
}

impl DesyncReplay for LockReplay {
    fn identity(&self) -> GameIdentity {
        GameIdentity::of::<LockGame>()
    }

    fn load(&mut self, snapshot: &BundleSnapshot) -> Result<(), String> {
        self.sim = match snapshot.tick {
            None if snapshot.data.0.is_empty() => LockSim::default(),
            _ => LockSim::load_state(&snapshot.data.0)?,
        };
        Ok(())
    }

    fn step(&mut self, frame: &BundleFrame) -> Result<(), String> {
        let inputs = frame.decode::<LockInput>().map_err(|e| e.to_string())?;
        let frame = LockstepFrame {
            tick: frame.tick,
            inputs: inputs
                .into_iter()
                .map(|(seat, input)| (SeatId(seat), input.map_or(SeatInput::Absent, SeatInput::Present)))
                .collect(),
            delay: 0,
            delay_from: 0,
            membership: frame
                .membership
                .iter()
                .map(|m| MembershipChange {
                    seat: SeatId(m.seat),
                    kind: if m.joins {
                        MembershipKind::Join
                    } else {
                        MembershipKind::Leave
                    },
                })
                .collect(),
            // The replay record carries no match end; it never changes a step.
            match_end: false,
        };
        self.sim.step(&frame);
        Ok(())
    }
}

/// Hashes and checkpoint snapshots one participant keeps of its simulation.
#[derive(Debug, Clone, Default)]
pub struct Track {
    /// The live simulation.
    pub sim: LockSim,
    /// The simulation after each checkpoint tick (the newest few).
    pub snapshots: BTreeMap<u64, LockSim>,
    /// The hash after each tick (the newest [`TRACK_HASHES`]).
    pub hashes: BTreeMap<u64, u64>,
    /// Ticks stepped, so far.
    pub stepped: u64,
    /// Whether every stepped tick followed the one before it and every replay was whole.
    pub in_order: bool,
}

/// Per-tick hashes a [`Track`] keeps.
pub const TRACK_HASHES: usize = 1_200;
/// Checkpoint snapshots a [`Track`] keeps.
pub const TRACK_SNAPSHOTS: usize = 16;

impl Track {
    /// An empty track.
    pub fn new() -> Self {
        Self {
            in_order: true,
            ..Self::default()
        }
    }

    /// Steps `frame` and returns the hash after it.
    pub fn step(&mut self, frame: &LockstepFrame<LockInput>, hash_every: u32) -> u64 {
        if frame.tick != self.sim.tick {
            self.in_order = false;
        }
        self.sim.step(frame);
        self.stepped += 1;
        let hash = self.sim.hash();
        self.note(frame.tick, hash, hash_every);
        hash
    }

    fn note(&mut self, tick: u64, hash: u64, hash_every: u32) {
        self.hashes.insert(tick, hash);
        while self.hashes.len() > TRACK_HASHES {
            self.hashes.pop_first();
        }
        if hash_every > 0 && tick.is_multiple_of(u64::from(hash_every)) {
            self.snapshots.insert(tick, self.sim.clone());
            while self.snapshots.len() > TRACK_SNAPSHOTS {
                self.snapshots.pop_first();
            }
        }
    }

    /// Takes `state`, a rollback runner's confirmed state after `tick`, as the state after that
    /// tick, as [`Self::step`] would have left it.
    pub fn mirror(&mut self, tick: u64, state: &LockSim, hash_every: u32) {
        if tick != self.sim.tick {
            self.in_order = false;
        }
        self.sim.clone_from(state);
        self.stepped += 1;
        let hash = self.sim.hash();
        self.note(tick, hash, hash_every);
    }

    /// Replays `frames` on top of the snapshot after `from` (`None`: the empty start), replacing
    /// the live simulation and the hashes of the replayed ticks. Returns `false` when the snapshot
    /// is gone, and when a frame does not continue the simulation (a frame is missing, or the
    /// frames start past the snapshot): the replay stops there and the track is no longer
    /// [`Self::in_order`], since its simulation is not the match's any more.
    pub fn replay<'a>(
        &mut self,
        from: Option<u64>,
        frames: impl Iterator<Item = &'a LockstepFrame<LockInput>>,
        hash_every: u32,
    ) -> bool {
        let mut sim = match from {
            None => LockSim::default(),
            Some(tick) => match self.snapshots.get(&tick) {
                Some(sim) => sim.clone(),
                None => return false,
            },
        };
        std::mem::swap(&mut self.sim, &mut sim);
        for frame in frames {
            if frame.tick != self.sim.tick {
                self.in_order = false;
                return false;
            }
            self.sim.step(frame);
            let hash = self.sim.hash();
            self.note(frame.tick, hash, hash_every);
        }
        true
    }

    /// Takes `state` as the simulation after `tick` (a state transfer) and replays `frames`
    /// (a replay that is not whole marks the track, see [`Self::replay`]).
    pub fn adopt<'a>(
        &mut self,
        tick: u64,
        state: LockSim,
        frames: impl Iterator<Item = &'a LockstepFrame<LockInput>>,
        hash_every: u32,
    ) -> u64 {
        let hash = state.hash();
        self.snapshots.insert(tick, state);
        self.hashes.insert(tick, hash);
        self.replay(Some(tick), frames, hash_every);
        hash
    }
}

/// The hashes `track` now holds for the ticks of `frames` (after a replay or a transfer): what
/// a game reports with `report_replayed_hash`, so the probe ring holds the replayed history.
fn replayed_hashes<'a>(
    track: &'a Track,
    frames: &'a [LockstepFrame<LockInput>],
) -> impl Iterator<Item = (u64, u64)> + 'a {
    frames
        .iter()
        .filter_map(|f| track.hashes.get(&f.tick).map(|h| (f.tick, *h)))
}

/// What the host process is in a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoakHost {
    /// A listen server: seat 0 is the host's own player; the other seats are clients.
    Listen,
    /// A dedicated server that steps its own simulation and votes as an observer.
    Observer,
    /// A dedicated server without a simulation.
    Relay,
}

/// One lockstep run.
#[derive(Debug, Clone)]
pub struct LockstepScenario {
    /// The network profile (a mix assigns each client its own).
    pub profile: Profile,
    /// Seats in the match.
    pub seats: usize,
    /// The seed of the network and the input scripts.
    pub seed: u64,
    /// Virtual milliseconds.
    pub duration_ms: u64,
    /// What the host is.
    pub host: SoakHost,
    /// The lockstep settings.
    pub config: LockstepConfig,
    /// How often inputs change.
    pub mix: InputMix,
    /// `(seat, tick)`: that seat's simulation is perturbed after it steps the tick.
    #[cfg(feature = "desync-inject")]
    pub drift: Option<(u16, u64)>,
    /// A seat that falls a frame log behind (an outage longer than the log) is sent a catch-up:
    /// the host's newest checkpoint snapshot, serialized, plus every frame since (spec 120 § 7.4).
    pub catch_up_lagging: bool,
    /// Record the host's `.lsreplay` with these settings (`None`: no recording; a relay host has
    /// no simulation and records nothing). The bytes land in [`LockstepRecord::replay`].
    pub replay: Option<LsReplaySettings>,
}

impl LockstepScenario {
    /// A run of `seats` seats on `profile` for `duration_ms`, with a dedicated observer host, the
    /// default settings and the order-driven mix.
    pub fn new(profile: Profile, seats: usize, seed: u64, duration_ms: u64) -> Self {
        Self {
            profile,
            seats,
            seed,
            duration_ms,
            host: SoakHost::Observer,
            config: LockstepConfig::default(),
            mix: InputMix::ORDERS,
            #[cfg(feature = "desync-inject")]
            drift: None,
            catch_up_lagging: true,
            replay: None,
        }
    }

    #[cfg(feature = "desync-inject")]
    fn drifts(&self, seat: u16, tick: u64) -> bool {
        self.drift == Some((seat, tick))
    }

    #[cfg(not(feature = "desync-inject"))]
    fn drifts(&self, _seat: u16, _tick: u64) -> bool {
        false
    }
}

#[cfg(feature = "desync-inject")]
fn perturb(sim: &mut LockSim) {
    sim.perturb();
}

#[cfg(not(feature = "desync-inject"))]
fn perturb(_sim: &mut LockSim) {}

/// One seat's measurements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatRecord {
    /// The seat.
    pub seat: u16,
    /// The link profile it ran on.
    pub profile: String,
    /// When its connection was accepted, virtual milliseconds.
    pub connected_ms: Option<u64>,
    /// How its connection ended, if it did.
    pub ended: Option<String>,
    /// Frames it stepped.
    pub frames_stepped: u64,
    /// Whether it stepped every tick in order, once.
    pub in_order: bool,
    /// The longest time between two stepped frames, virtual milliseconds.
    pub max_step_gap_ms: u64,
    /// The seat's `net.lockstep` status at the end.
    pub status: LockstepStatus,
    /// Frames it got from the stream.
    pub stream_frames: u64,
    /// Self-check replays it ran.
    pub self_checks: u32,
    /// State transfers it took.
    pub resyncs: u32,
    /// Payloads from the host it refused.
    pub refused: u64,
    /// Inputs the seat's lockstep core refused to stamp.
    pub inputs_refused: u64,
    /// Catch-ups it took (state plus frames, spec 120 § 7.4).
    #[serde(default)]
    pub catchups: u32,
    /// Ticks of subsystem rows the seat's core holds at the end (the desync probe's ring).
    #[serde(default)]
    pub subsystem_rows: usize,
    /// Address changes the seat's link went through in the run (NAT rebindings and address
    /// moves of its profile).
    #[serde(default)]
    pub nat_rebindings: u64,
    /// Validated moves of the seat's connection to a new address, from the host's peer row
    /// (ADR-8299).
    #[serde(default)]
    pub rebinds: u64,
    /// Records from a new address of the seat that the host did not let in (ADR-8299; since
    /// ADR-8422 an address that authenticated is past the trial budget).
    #[serde(default)]
    pub rebinds_refused: u64,
    /// Packets the host also sent to the seat's new address while its old one was silent
    /// (ADR-8422).
    #[serde(default)]
    pub path_mirrored: u64,
}

/// What one lockstep run measured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockstepRecord {
    /// Per-seat measurements (remote seats).
    pub seats: Vec<SeatRecord>,
    /// The host's `net.lockstep` status at the end.
    pub host: LockstepStatus,
    /// Frames the host built.
    pub frames_built: u64,
    /// The mean input delay over the frames built, thousandths of a tick.
    #[serde(default)]
    pub delay_mean_milliticks: u64,
    /// The largest input delay a frame was built under, ticks.
    #[serde(default)]
    pub delay_max_ticks: u16,
    /// When the host started the frame clock, virtual milliseconds.
    pub started_ms: Option<u64>,
    /// Every desync verdict.
    pub verdicts: Vec<DesyncVerdict>,
    /// `(checkpoint tick, voter, first diverged tick)` from the probe.
    pub located: Vec<(u64, String, u64)>,
    /// The probe's reports: the first diverged subsystem and the last agreed tick per diverged
    /// voter (spec 120 § 7.6).
    #[serde(default)]
    pub probes: Vec<DesyncReport>,
    /// Seats removed for divergence.
    pub removed: Vec<u16>,
    /// State transfers the host's own simulation took.
    pub host_resyncs: u32,
    /// The votes could not be decided.
    pub unresolved: bool,
    /// Every simulation agrees at the last tick they all stepped.
    pub in_step: bool,
    /// The host's upload over the run after the start, bits per second, IP and UDP headers
    /// included.
    pub host_upload_bps: u64,
    /// The host's upload, datagrams per second.
    pub host_upload_datagrams_per_s: u64,
    /// Seat payloads the host refused after it started the frame clock.
    pub host_refused: u64,
    /// Seat payloads the host refused with `lockstep_not_started`: a seat that connected before
    /// the last one sends before the frame clock starts, and sends its inputs again after.
    pub host_refused_before_start: u64,
    /// Every refusal by error code, for the report.
    pub host_refused_codes: BTreeMap<String, u64>,
    /// Payloads the transport refused (host and seats).
    pub transport_refused: u64,
    /// The host's upload in its busiest whole second after the start, bits per second, IP and
    /// UDP headers included.
    pub host_upload_peak_bps: u64,
    /// The host's upload in the 95th-percentile whole second after the start (nearest rank), bits
    /// per second, IP and UDP headers included: what the `lockstep_upload` bar is judged on, with
    /// the busiest second (spec 120 § 7.5; ADR-8209).
    #[serde(default)]
    pub host_upload_p95_bps: u64,
    /// The host's upload in the median whole second after the start, bits per second.
    #[serde(default)]
    pub host_upload_p50_bps: u64,
    /// Whole seconds after the start the upload figures are taken over.
    #[serde(default)]
    pub host_upload_seconds: u64,
    /// Of those, seconds over [`UPLOAD_P95_LIMIT_BPS`].
    #[serde(default)]
    pub host_upload_seconds_over: u64,
    /// Inputs of a player host that its lockstep core refused to stamp.
    pub host_inputs_refused: u64,
    /// Seats the host stopped feeding because they fell a frame log behind.
    pub lagging: Vec<u16>,
    /// State transfers that could not be applied (the source had no snapshot at the tick).
    pub transfers_skipped: u32,
    /// Catch-ups the host sent to lagging seats.
    #[serde(default)]
    pub catchups: u32,
    /// Catch-ups the host could not send (no snapshot, or the frame log no longer covers it).
    #[serde(default)]
    pub catchups_refused: u32,
    /// Whether the host's own simulation stepped every tick in order, once, and every replay it
    /// ran was whole; `None` for a relay host, which has no simulation.
    #[serde(default)]
    pub host_in_order: Option<bool>,
    /// The last tick every simulation stepped, which [`Self::in_step`] is judged at.
    #[serde(default)]
    pub common_tick: Option<u64>,
    /// The state hash every simulation has after [`Self::common_tick`] (`None` when they differ).
    #[serde(default)]
    pub common_hash: Option<u64>,
    /// What the host's transport sent to its seats over the whole run (every connection's
    /// `LinkStats`, read at the end): packets, packet body bytes and, of those, acknowledgment
    /// frame bytes. The per-datagram breakdown of the upload (ADR-8423).
    #[serde(default)]
    pub host_link: HostLinkTotals,
    /// The host's `.lsreplay` when the scenario asked for one (not serialized).
    #[serde(skip)]
    pub replay: Option<Vec<u8>>,
}

/// The host transport's totals over a lockstep run (see `LockstepRecord::host_link`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostLinkTotals {
    /// Packets sent.
    pub packets: u64,
    /// Packet body bytes sent (the endpoint header, the seal and IP/UDP headers excluded).
    pub body_bytes: u64,
    /// Of those, acknowledgment frame bytes.
    pub ack_frame_bytes: u64,
}

fn server_addr() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], 7_778))
}

fn client_addr(i: usize) -> SocketAddr {
    SocketAddr::from(([10, 2 + (i / 250) as u8, 0, (i % 250) as u8 + 1], 41_000))
}

/// The `lockstep_upload` bar (spec 120 § 7.5 as amended by ADR-8209): the host's upload in the
/// 95th-percentile whole second of a 5-minute run, bits per second, IP and UDP headers included.
pub const UPLOAD_P95_LIMIT_BPS: u64 = 4_000_000;
/// The `lockstep_upload` bar: the host's upload in the busiest whole second of the same run.
pub const UPLOAD_PEAK_LIMIT_BPS: u64 = 4_400_000;
/// The run length the `lockstep_upload` bar is stated for, virtual milliseconds.
pub const UPLOAD_BAR_RUN_MS: u64 = 300_000;
/// Seats the `lockstep_upload` bar is stated for on the order-driven mix ([`InputMix::ORDERS`]):
/// `report::LOCKSTEP_UPLOAD_SEATS_ORDERS`, the one place the number is written.
pub const UPLOAD_BAR_SEATS_ORDERS: usize = crate::report::LOCKSTEP_UPLOAD_SEATS_ORDERS;
/// Seats the `lockstep_upload` bar is stated for on the action mix ([`InputMix::ACTION`]): the
/// largest of the measured 16, 24 and 40 seats that holds it (spec 120 § 7.5);
/// `report::LOCKSTEP_UPLOAD_SEATS_ACTION`.
pub const UPLOAD_BAR_SEATS_ACTION: usize = crate::report::LOCKSTEP_UPLOAD_SEATS_ACTION;

/// A client frame every this many virtual milliseconds.
const FRAME_MS: u64 = 16;

struct SeatNode {
    seat: SeatId,
    client: NetClient<LockGame, ImpairedSocket>,
    core: LockstepSeat<LockGame>,
    /// Under [`LockstepMode::Rollback`]: the seat predicts and rolls back; `track` mirrors its
    /// confirmed state.
    runner: Option<RollbackRunner<LockGame, LockSim>>,
    track: Track,
    input: InputSource,
    phase: u64,
    connected: bool,
    first_sent: bool,
    last_step_ms: Option<u64>,
    rec: SeatRecord,
}

/// Runs `s` and returns its measurements.
pub fn run_lockstep(s: &LockstepScenario) -> io::Result<LockstepRecord> {
    let _span =
        tracing::info_span!("net_soak_lockstep", seats = s.seats, profile = %s.profile.name).entered();
    let config = s.config.clone().sanitized();
    let hash_every = config.hash_every;
    let net = ImpairedNet::new(s.seed);
    let listen = s.host == SoakHost::Listen;
    let server_ep = net.add_endpoint(server_addr(), s.profile.server_link(listen))?;
    let mut rng = SoakRng::derive(s.seed, 7);
    let words = [rng.next_u64(), rng.next_u64(), rng.next_u64(), rng.next_u64()];
    let mut secret = [0u8; 32];
    for (i, b) in secret.iter_mut().enumerate() {
        *b = (words[i / 8] >> ((i % 8) * 8)) as u8;
    }
    let mut server = NetServer::<LockGame, _>::with_secret(
        net.socket(server_ep),
        ServerConfig {
            max_peers: s.seats + 4,
            tick_rate_hz: config.tick_rate_hz,
            ..ServerConfig::default()
        },
        secret,
    );
    let role = match s.host {
        SoakHost::Listen => HostRole::Listen(SeatId(0)),
        SoakHost::Observer => HostRole::Observer,
        SoakHost::Relay => HostRole::Relay,
    };
    let mut host = LockstepHost::<LockGame>::new(config.clone(), role);
    let rollback = matches!(config.mode, LockstepMode::Rollback { .. });
    if rollback {
        host.set_input_rules(lock_input_rules());
    }
    let mut host_track = (s.host != SoakHost::Relay).then(Track::new);
    let mut host_input = listen.then(|| InputSource::new(s.seed, 0, s.mix));
    let first_remote = usize::from(listen);
    if listen {
        host.add_seat(SeatId(0));
    }
    let mut nodes: Vec<SeatNode> = Vec::with_capacity(s.seats);
    for i in first_remote..s.seats {
        let seat = SeatId(i as u16);
        host.add_seat(seat);
        let link = s.profile.client_link(i, s.duration_ms);
        let nat_rebindings = (link.rebinds_ms.iter().filter(|&&t| t <= s.duration_ms).count()
            + link.ip_moves.iter().filter(|&&(t, _)| t <= s.duration_ms).count())
            as u64;
        let ep = net.add_endpoint(client_addr(i), link)?;
        let client = NetClient::<LockGame, _>::connect(
            net.socket(ep),
            server_addr(),
            ClientConfig {
                name: format!("seat{i}"),
                ..ClientConfig::default()
            },
            &(i as u32),
        )
        .map_err(|e| io::Error::other(format!("connect: {e:?}")))?;
        let mut core = LockstepSeat::new(config.clone(), seat);
        if rollback {
            core.set_input_rules(lock_input_rules());
        }
        nodes.push(SeatNode {
            seat,
            client,
            core,
            runner: rollback.then(|| RollbackRunner::new(&config, seat, LockSim::default())),
            track: Track::new(),
            input: InputSource::new(s.seed, seat.0, s.mix),
            phase: (i as u64 * 5) % FRAME_MS,
            connected: false,
            first_sent: false,
            last_step_ms: None,
            rec: SeatRecord {
                seat: seat.0,
                profile: s.profile.client_profile(i),
                connected_ms: None,
                ended: None,
                frames_stepped: 0,
                in_order: true,
                max_step_gap_ms: 0,
                status: LockstepStatus::default(),
                stream_frames: 0,
                self_checks: 0,
                resyncs: 0,
                refused: 0,
                inputs_refused: 0,
                catchups: 0,
                subsystem_rows: 0,
                nat_rebindings,
                rebinds: 0,
                rebinds_refused: 0,
                path_mirrored: 0,
            },
        });
    }
    let mut seat_of: BTreeMap<PeerId, SeatId> = BTreeMap::new();
    let mut peer_of: BTreeMap<SeatId, PeerId> = BTreeMap::new();
    let mut rec = LockstepRecord {
        seats: Vec::new(),
        host: LockstepStatus::default(),
        frames_built: 0,
        delay_mean_milliticks: 0,
        delay_max_ticks: 0,
        started_ms: None,
        verdicts: Vec::new(),
        located: Vec::new(),
        probes: Vec::new(),
        removed: Vec::new(),
        host_resyncs: 0,
        unresolved: false,
        in_step: false,
        host_upload_bps: 0,
        host_upload_datagrams_per_s: 0,
        host_refused: 0,
        host_refused_before_start: 0,
        host_refused_codes: BTreeMap::new(),
        transport_refused: 0,
        host_upload_peak_bps: 0,
        host_upload_p95_bps: 0,
        host_upload_p50_bps: 0,
        host_upload_seconds: 0,
        host_upload_seconds_over: 0,
        host_inputs_refused: 0,
        lagging: Vec::new(),
        transfers_skipped: 0,
        catchups: 0,
        catchups_refused: 0,
        host_in_order: None,
        host_link: HostLinkTotals::default(),
        common_tick: None,
        common_hash: None,
        replay: None,
    };
    // The host's replay (ADR-8513): its own simulation's frames, Fulls and checkpoint hashes.
    let mut replay = match (&s.replay, host_track.is_some()) {
        (Some(settings), true) if settings.record => {
            let header = LsReplayHeader::of::<LockGame>(
                (0..s.seats).map(|i| SeatId(i as u16)),
                config.tick_rate_hz,
                s.seed,
                settings,
            );
            LsReplayWriter::new(Vec::new(), header, settings.clone()).ok()
        }
        _ => None,
    };
    // State transfers the events ask for, applied between the seat pass and the host pass.
    let mut transfers: Vec<(Voter, Voter, u64)> = Vec::new();

    let mut delay_sum = 0u64;
    let mut now = 0u64;
    while now <= s.duration_ms {
        net.advance_to_ms(now);
        // Seats.
        for node in &mut nodes {
            if now % FRAME_MS != node.phase {
                continue;
            }
            node.client.receive(now);
            while let Some(event) = node.client.poll_event() {
                match event {
                    ClientEvent::Connected { .. } => {
                        node.connected = true;
                        node.rec.connected_ms = Some(now);
                    }
                    ClientEvent::Ended(end) => {
                        node.connected = false;
                        node.rec.ended = Some(end.as_str().to_owned());
                    }
                    ClientEvent::Message { channel, message } => {
                        if let Some(Err(_)) = seat_receive(&mut node.core, channel, &message, now) {
                            node.rec.refused += 1;
                        }
                    }
                    _ => {}
                }
            }
            if node.connected {
                if let Some(runner) = node.runner.as_mut() {
                    // Rollback: the runner steps the frames, predicts ahead and samples one input per
                    // tick it steps; the track mirrors the confirmed state for votes and transfers.
                    let (input, track) = (&mut node.input, &mut node.track);
                    let report = runner.advance(
                        &mut node.core,
                        now,
                        |_| input.next_input(),
                        |tick, state| track.mirror(tick, state, hash_every),
                    );
                    node.rec.inputs_refused += u64::from(report.inputs_refused);
                    if report.confirmed.end > report.confirmed.start {
                        if let Some(last) = node.last_step_ms {
                            node.rec.max_step_gap_ms = node.rec.max_step_gap_ms.max(now - last);
                        }
                        node.last_step_ms = Some(now);
                    }
                } else {
                    if !node.first_sent {
                        node.first_sent = true;
                        let motion = node.track.sim.motion();
                        if node
                            .core
                            .submit_input(&node.input.next_input_with(&motion))
                            .is_err()
                        {
                            node.rec.inputs_refused += 1;
                        }
                    }
                    while let Some(frame) = node.core.next_frame() {
                        let mut hash = node.track.step(&frame, hash_every);
                        if s.drifts(node.seat.0, frame.tick) {
                            perturb(&mut node.track.sim);
                            hash = node.track.sim.hash();
                            node.track.hashes.insert(frame.tick, hash);
                            if let Some(snap) = node.track.snapshots.get_mut(&frame.tick) {
                                *snap = node.track.sim.clone();
                            }
                        }
                        let world = node.core.record_state(frame.tick, &node.track.sim);
                        debug_assert_eq!(world, hash);
                        let motion = node.track.sim.motion();
                        if node
                            .core
                            .submit_input(&node.input.next_input_with(&motion))
                            .is_err()
                        {
                            node.rec.inputs_refused += 1;
                        }
                        if let Some(last) = node.last_step_ms {
                            node.rec.max_step_gap_ms = node.rec.max_step_gap_ms.max(now - last);
                        }
                        node.last_step_ms = Some(now);
                    }
                }
                while let Some(event) = node.core.poll_event() {
                    match event {
                        SeatEvent::SelfCheck { from, tick } => {
                            node.rec.self_checks += 1;
                            let frames: Vec<_> = node
                                .core
                                .frames_since(from.map_or(0, |t| t + 1))
                                .cloned()
                                .collect();
                            let whole = node.track.replay(from, frames.iter(), hash_every);
                            for (t, h) in replayed_hashes(&node.track, &frames) {
                                node.core.report_replayed_hash(t, h);
                            }
                            if whole && let Some(hash) = node.track.hashes.get(&tick) {
                                node.core.revote(tick, *hash);
                            }
                            if let Some(runner) = node.runner.as_mut() {
                                runner.reset(node.track.sim.tick, node.track.sim.clone());
                            }
                        }
                        SeatEvent::Resync { tick, source } => {
                            transfers.push((Voter::Seat(node.seat), source, tick));
                        }
                        SeatEvent::StateReady { .. } => {
                            // A catch-up: load the host's state, then step the frames after it.
                            if let Some((tick, bytes)) = node.core.take_state()
                                && let Ok(state) = serde_json::from_slice::<LockSim>(&bytes)
                            {
                                node.track.adopt(tick, state, std::iter::empty(), hash_every);
                                node.track.stepped = tick + 1;
                                node.rec.catchups += 1;
                                if let Some(runner) = node.runner.as_mut() {
                                    runner.reset(node.track.sim.tick, node.track.sim.clone());
                                }
                            } else {
                                node.rec.refused += 1;
                            }
                        }
                        _ => {}
                    }
                }
                let report = send_seat(&mut node.client, &mut node.core, now);
                rec.transport_refused += u64::from(report.datagrams_refused + report.stream_refused);
            }
            node.client.flush(now);
        }
        // Host.
        server.receive(now);
        while let Some(event) = server.poll_event() {
            match event {
                ServerEvent::PeerJoined { peer, join, .. } => {
                    let seat = SeatId(join as u16);
                    seat_of.insert(peer, seat);
                    peer_of.insert(seat, peer);
                }
                ServerEvent::Message {
                    peer,
                    channel,
                    message,
                } => {
                    if let Some(seat) = seat_of.get(&peer)
                        && let Some(Err(err)) = host_receive(&mut host, *seat, channel, &message, now)
                    {
                        *rec.host_refused_codes.entry(err.code().to_owned()).or_default() += 1;
                        if !host.started() {
                            rec.host_refused_before_start += 1;
                        } else {
                            rec.host_refused += 1;
                        }
                    }
                }
                _ => {}
            }
        }
        // Once a second: the transport's round trip per connection, for the late-stamp watch.
        if now.is_multiple_of(1_000) {
            for row in server.peer_rows() {
                if let Some(seat) = seat_of.get(&PeerId(row.id)) {
                    host.note_transport_rtt(*seat, row.rtt_ms as u64);
                }
            }
        }
        if !host.started() && peer_of.len() == nodes.len() {
            host.start(now);
            rec.started_ms = Some(now);
            if let (Some(writer), Some(track)) = (replay.as_mut(), host_track.as_ref())
                && writer
                    .record_start(&track.sim, |o| track.sim.save_state(o))
                    .is_err()
            {
                replay = None;
            }
            if host_input.is_some() {
                let motion = host_track
                    .as_ref()
                    .map(|track| track.sim.motion())
                    .unwrap_or_default();
                if let Some(input) = host_input.as_mut()
                    && host.local_input(&input.next_input_with(&motion), now).is_err()
                {
                    rec.host_inputs_refused += 1;
                }
            }
        }
        let mut built = false;
        while let Some(frame) = host.poll_frame(now) {
            built = true;
            rec.frames_built += 1;
            let delay = host.delay_ticks();
            delay_sum += u64::from(delay);
            rec.delay_max_ticks = rec.delay_max_ticks.max(delay);
            if let Some(writer) = replay.as_mut()
                && writer.record_frame(&frame).is_err()
            {
                replay = None;
            }
            if let Some(track) = host_track.as_mut() {
                let mut hash = track.step(&frame, hash_every);
                if s.drifts(u16::MAX, frame.tick) {
                    perturb(&mut track.sim);
                    hash = track.sim.hash();
                    track.hashes.insert(frame.tick, hash);
                    if let Some(snap) = track.snapshots.get_mut(&frame.tick) {
                        *snap = track.sim.clone();
                    }
                }
                let world = host.record_state(frame.tick, &track.sim);
                debug_assert_eq!(world, hash);
                if let Some(writer) = replay.as_mut()
                    && writer
                        .record_state(frame.tick, &track.sim, |o| track.sim.save_state(o))
                        .is_err()
                {
                    replay = None;
                }
            }
            if host_input.is_some() {
                let motion = host_track
                    .as_ref()
                    .map(|track| track.sim.motion())
                    .unwrap_or_default();
                if let Some(input) = host_input.as_mut()
                    && host.local_input(&input.next_input_with(&motion), now).is_err()
                {
                    rec.host_inputs_refused += 1;
                }
            }
        }
        while let Some(event) = host.poll_event() {
            match event {
                HostEvent::Desync(verdict) => rec.verdicts.push(verdict),
                HostEvent::DesyncLocated {
                    tick,
                    voter,
                    first_diverged_tick,
                } => rec.located.push((tick, voter.to_string(), first_diverged_tick)),
                HostEvent::DesyncProbed(report) => rec.probes.push(report),
                HostEvent::SelfCheck { from, tick } => {
                    if let Some(track) = host_track.as_mut() {
                        let frames: Vec<_> = host.frames_since(from.map_or(0, |t| t + 1)).cloned().collect();
                        let whole = track.replay(from, frames.iter(), hash_every);
                        for (t, h) in replayed_hashes(track, &frames) {
                            host.report_replayed_hash(t, h);
                        }
                        if whole && let Some(hash) = track.hashes.get(&tick) {
                            host.revote(tick, *hash);
                        }
                    }
                }
                HostEvent::ResyncHost { source, tick, .. } => {
                    rec.host_resyncs += 1;
                    let host_voter = match role {
                        HostRole::Listen(seat) => Voter::Seat(seat),
                        _ => Voter::Observer,
                    };
                    transfers.push((host_voter, source, tick));
                }
                HostEvent::SeatRemoved { seat, .. } => rec.removed.push(seat.0),
                HostEvent::SeatLagging { seat } => {
                    rec.lagging.push(seat.0);
                    // The newest checkpoint the host's own simulation holds, as the game's bytes.
                    let state = host_track.as_ref().and_then(|t| t.snapshots.last_key_value());
                    let sent = match state.map(|(tick, sim)| (*tick, serde_json::to_vec(sim))) {
                        Some((tick, Ok(bytes))) if s.catch_up_lagging => {
                            host.catch_up(seat, tick, &bytes, now).is_ok()
                        }
                        _ => false,
                    };
                    if sent {
                        rec.catchups += 1;
                    } else {
                        rec.catchups_refused += 1;
                    }
                }
                HostEvent::Unresolved { .. } => rec.unresolved = true,
                _ => {}
            }
        }
        // The state transfers (the late-join path's job in a game): copy the source's snapshot at
        // the tick, replay the target's own frames after it, and vote again.
        for (target, source, tick) in transfers.drain(..) {
            let host_voter = match role {
                HostRole::Listen(seat) => Voter::Seat(seat),
                _ => Voter::Observer,
            };
            let state = if source == host_voter {
                host_track.as_ref().and_then(|t| t.snapshots.get(&tick).cloned())
            } else {
                nodes
                    .iter()
                    .find(|n| Voter::Seat(n.seat) == source)
                    .and_then(|n| n.track.snapshots.get(&tick).cloned())
            };
            let Some(state) = state else {
                rec.transfers_skipped += 1;
                continue;
            };
            if target == host_voter {
                if let Some(track) = host_track.as_mut() {
                    let frames: Vec<_> = host.frames_since(tick + 1).cloned().collect();
                    let hash = track.adopt(tick, state, frames.iter(), hash_every);
                    for (t, h) in replayed_hashes(track, &frames) {
                        host.report_replayed_hash(t, h);
                    }
                    host.revote(tick, hash);
                }
            } else if let Some(node) = nodes.iter_mut().find(|n| Voter::Seat(n.seat) == target) {
                node.rec.resyncs += 1;
                let frames: Vec<_> = node.core.frames_since(tick + 1).cloned().collect();
                let hash = node.track.adopt(tick, state, frames.iter(), hash_every);
                for (t, h) in replayed_hashes(&node.track, &frames) {
                    node.core.report_replayed_hash(t, h);
                }
                node.core.resynced(tick, hash);
                if let Some(runner) = node.runner.as_mut() {
                    runner.reset(node.track.sim.tick, node.track.sim.clone());
                }
            }
        }
        // A host loop sends once per tick: the transport's acknowledgements ride with the frames.
        if built {
            let report = send_host(&mut server, &mut host, |seat| peer_of.get(&seat).copied(), now);
            rec.transport_refused += u64::from(report.datagrams_refused + report.stream_refused);
        }
        if built || !host.started() {
            server.flush(now);
        }
        now += 1;
    }

    // Every simulation agrees at the last tick they all stepped.
    let mut common = host_track.as_ref().map_or(u64::MAX, |t| t.stepped);
    for node in &nodes {
        if !rec.removed.contains(&node.seat.0) {
            common = common.min(node.track.stepped);
        }
    }
    let mut hashes = std::collections::BTreeSet::new();
    if common > 0 && common != u64::MAX {
        let tick = common - 1;
        if let Some(track) = &host_track {
            hashes.insert(track.hashes.get(&tick).copied());
        }
        for node in &nodes {
            if !rec.removed.contains(&node.seat.0) {
                hashes.insert(node.track.hashes.get(&tick).copied());
            }
        }
    }
    rec.in_step = hashes.len() == 1 && !hashes.contains(&None);
    if common > 0 && common != u64::MAX {
        rec.common_tick = Some(common - 1);
        rec.common_hash = if rec.in_step {
            hashes.first().copied().flatten()
        } else {
            None
        };
    }

    let stats = net.stats(server_ep);
    let from_s = rec.started_ms.map_or(0, |ms| ms / 1000 + 2) as usize;
    let seconds = stats.up_offered_per_s.len().saturating_sub(from_s + 1);
    if seconds > 0 {
        let bytes: u64 = stats.up_offered_per_s.iter().skip(from_s).take(seconds).sum();
        let datagrams: u64 = stats
            .up_offered_datagrams_per_s
            .iter()
            .skip(from_s)
            .take(seconds)
            .sum();
        rec.host_upload_bps = (bytes + datagrams * IP_UDP_OVERHEAD as u64) * 8 / seconds as u64;
        rec.host_upload_datagrams_per_s = datagrams / seconds as u64;
        let mut per_second: Vec<u64> = stats
            .up_offered_per_s
            .iter()
            .zip(&stats.up_offered_datagrams_per_s)
            .skip(from_s)
            .take(seconds)
            .map(|(bytes, datagrams)| (bytes + datagrams * IP_UDP_OVERHEAD as u64) * 8)
            .collect();
        per_second.sort_unstable();
        let rank = |per_cent: usize| {
            let index = (per_second.len() * per_cent).div_ceil(100).saturating_sub(1);
            per_second.get(index).copied().unwrap_or(0)
        };
        rec.host_upload_peak_bps = per_second.last().copied().unwrap_or(0);
        rec.host_upload_p95_bps = rank(95);
        rec.host_upload_p50_bps = rank(50);
        rec.host_upload_seconds = per_second.len() as u64;
        rec.host_upload_seconds_over = per_second
            .iter()
            .filter(|bps| **bps > UPLOAD_P95_LIMIT_BPS)
            .count() as u64;
    }
    rec.host_in_order = host_track.as_ref().map(|t| t.in_order);
    for row in server.peer_rows() {
        rec.host_link.packets += row.link.packets_sent;
        rec.host_link.body_bytes += row.link.bytes_sent;
        rec.host_link.ack_frame_bytes += row.link.ack_frame_bytes;
    }
    rec.replay = replay.and_then(|writer| {
        let abort = (rec.unresolved || host.unresolved()).then(|| {
            ReplayAbortInfo::new(host.tick(), "desync_unresolved", "the votes could not be decided")
        });
        writer.finish(abort).ok()
    });
    rec.delay_mean_milliticks = (delay_sum * 1000).checked_div(rec.frames_built).unwrap_or(0);
    rec.host = host.status(s.duration_ms);
    rec.unresolved |= host.unresolved();
    let rows = server.peer_rows();
    for mut node in nodes {
        if let Some(row) = peer_of
            .get(&node.seat)
            .and_then(|peer| rows.iter().find(|row| row.id == peer.0))
        {
            node.rec.rebinds = row.rebinds;
            node.rec.rebinds_refused = row.rebinds_refused;
            node.rec.path_mirrored = row.path_mirrored;
        }
        node.rec.frames_stepped = node.track.stepped;
        node.rec.in_order = node.track.in_order;
        node.rec.status = node.core.status();
        node.rec.stream_frames = node.core.stream_stats().0;
        node.rec.subsystem_rows = node.core.subsystem_rows().len();
        rec.seats.push(node.rec);
    }
    Ok(rec)
}

#[cfg(test)]
mod tests {
    use regia_net::lockstep::MembershipChange;

    use super::*;

    const HASH_EVERY: u32 = 30;

    fn frame(tick: u64) -> LockstepFrame<LockInput> {
        let input = |seat: u16| LockInput {
            throttle: 127,
            aim: (tick as u16)
                .wrapping_mul(37)
                .wrapping_add(seat.wrapping_mul(9001)),
            buttons: u16::from(tick.is_multiple_of(40)),
            ..LockInput::default()
        };
        LockstepFrame {
            tick,
            inputs: (0..3)
                .map(|s| (SeatId(s), SeatInput::Present(input(s))))
                .collect(),
            delay: 4,
            delay_from: 0,
            membership: if tick == 0 {
                (0..3)
                    .map(|s| MembershipChange {
                        seat: SeatId(s),
                        kind: MembershipKind::Join,
                    })
                    .collect()
            } else {
                Vec::new()
            },
            match_end: false,
        }
    }

    fn stepped(ticks: u64) -> (Track, Vec<LockstepFrame<LockInput>>) {
        let frames: Vec<_> = (0..ticks).map(frame).collect();
        let mut track = Track::new();
        for f in &frames {
            track.step(f, HASH_EVERY);
        }
        (track, frames)
    }

    /// A replay that is handed every frame after the snapshot ends in the state the live
    /// simulation had, with the same hashes.
    #[test]
    fn a_whole_replay_from_a_snapshot_reproduces_the_live_hashes() {
        let (mut track, frames) = stepped(70);
        let live = track.hashes.clone();
        let live_sim = track.sim.clone();
        assert!(track.replay(Some(30), frames.iter().skip(31), HASH_EVERY));
        assert_eq!(track.hashes, live);
        assert_eq!(track.sim, live_sim);
        assert!(track.in_order);
        // From the empty start.
        assert!(track.replay(None, frames.iter(), HASH_EVERY));
        assert_eq!((track.hashes, track.sim), (live, live_sim));
    }

    /// A replay whose frames do not continue the snapshot (the frame log no longer reaches back
    /// to it) is not a replay: it says so and the track is no longer in order. It used to skip the
    /// frames silently and answer `true` with the simulation left at the snapshot.
    #[test]
    fn a_replay_with_a_missing_frame_is_refused_and_marks_the_track() {
        let (mut track, frames) = stepped(70);
        assert!(
            !track.replay(Some(30), frames.iter().skip(32), HASH_EVERY),
            "frame 31 is missing: the frames do not continue the snapshot"
        );
        assert!(!track.in_order, "the track says its simulation is not whole");
        // A hole in the middle is the same.
        let (mut track, frames) = stepped(70);
        let holed = frames.iter().skip(31).filter(|f| f.tick != 50);
        assert!(!track.replay(Some(30), holed, HASH_EVERY));
        assert!(!track.in_order);
        // A snapshot that is gone is refused as before, and says nothing about the order.
        let (mut track, frames) = stepped(70);
        assert!(!track.replay(Some(45), frames.iter().skip(46), HASH_EVERY));
        assert!(track.in_order);
    }

    /// The state hash folds the tick, so it differs between two ticks even when no hull moved;
    /// the body hash is the hulls alone.
    #[test]
    fn the_body_hash_ignores_the_tick_and_the_state_hash_does_not() {
        let (track, _) = stepped(10);
        let mut later = track.sim.clone();
        later.tick += 1;
        later.hits += 3;
        assert_ne!(track.sim.hash(), later.hash());
        assert_eq!(
            track.sim.body_hash(),
            later.body_hash(),
            "no hull changed: the body hash is the same"
        );
        let mut moved = track.sim.clone();
        if let Some(hull) = moved.hulls.values_mut().next() {
            hull.x += 0.25;
        }
        assert_ne!(track.sim.body_hash(), moved.body_hash());
    }

    /// A moving hull is led: the aim is ahead of the static bearing, a stationary hull is aimed
    /// straight at, and one hold keeps the same aim word.
    #[test]
    fn wander_fire_leads_a_moving_hull_and_holds_its_aim() {
        let moving = [
            (0, 0.0, 0.0, 0.0, 0.0),
            (1, 40.0, 0.0, core::f32::consts::FRAC_PI_2, 12.0),
        ];
        let mut src = InputSource::new(1, 0, InputMix::WANDER_FIRE);
        let led = src.next_input_with(&moving);
        assert!(
            led.aim > 500 && led.aim < 4_000,
            "lead is ahead of the static bearing, aim {}",
            led.aim
        );
        let still = [(0, 0.0, 0.0, 0.0, 0.0), (1, 40.0, 0.0, 0.0, 0.0)];
        let mut src = InputSource::new(1, 0, InputMix::WANDER_FIRE);
        let aim = src.next_input_with(&still).aim;
        assert!(
            !(50..=65_000).contains(&aim),
            "a stationary hull is aimed straight at, aim {aim}"
        );
        let mut held = InputSource::new(7, 3, InputMix::WANDER_FIRE);
        let frames: Vec<_> = (0..60).map(|_| held.next_input()).collect();
        assert!(frames.windows(2).all(|pair| pair[0].aim == pair[1].aim));
        assert!(frames.iter().all(|frame| frame.throttle == 127));
    }
}
