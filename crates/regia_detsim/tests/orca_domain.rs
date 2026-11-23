// SPDX-License-Identifier: MPL-2.0
//! Input domain, allocation-free scratch form and spatial-index neighbors for the
//! integer avoidance kernel (ADR-8377).
//!
//! Run in both profiles: `cargo test -p regia_detsim --test orca_domain` and the
//! same with `--release`. The out-of-domain test asserts the documented fallback
//! value, so a debug overflow panic or a release wrap fails it.
//!
//! Installs a counting global allocator (thread-local counter, opt-in per thread).

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_detsim::avoid::{
    ORCA_MAX_HORIZON_MS, ORCA_MAX_NEIGHBOR_PLANES, ORCA_MAX_POSITION_MM, ORCA_MAX_RADIUS_MM,
    ORCA_MAX_RANGE_MM, ORCA_MAX_SPEED_MM_S, OrcaBody, OrcaDomainError, OrcaInput, OrcaNeighborQuery,
    OrcaScratch, OrcaSettings, orca_velocity, orca_velocity_indexed, orca_velocity_into,
    orca_velocity_with_settings, try_orca_velocity, try_orca_velocity_indexed,
};
use regia_detsim::broadphase::{BroadBody, BroadphaseConfig};
use regia_detsim::spatial::{DetSpatial, SpatialRow};

// ---------------------------------------------------------------------------
// Counting allocator: counts only allocations made on a thread that opted in.

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

fn note_alloc() {
    let _ = COUNTING.try_with(|on| {
        if on.get() {
            let _ = ALLOCS.try_with(|n| n.set(n.get() + 1));
        }
    });
}

// SAFETY: forwards every call unchanged to the system allocator; the counter
// touches only const-initialized thread-local cells, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_alloc();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_alloc();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_alloc();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn count_allocs<R>(f: impl FnOnce() -> R) -> (R, u64) {
    ALLOCS.with(|n| n.set(0));
    COUNTING.with(|on| on.set(true));
    let r = f();
    COUNTING.with(|on| on.set(false));
    (r, ALLOCS.with(Cell::get))
}

// ---------------------------------------------------------------------------
// Shared helpers.

const P: i64 = ORCA_MAX_POSITION_MM;
const V: i64 = ORCA_MAX_SPEED_MM_S;
const R: i64 = ORCA_MAX_RADIUS_MM;

fn body(id: u32, x_mm: i64, z_mm: i64, vx_mm_s: i64, vz_mm_s: i64, r_mm: i64) -> OrcaBody {
    OrcaBody {
        id,
        x_mm,
        z_mm,
        vx_mm_s,
        vz_mm_s,
        r_mm,
    }
}

/// The documented out-of-domain value: the preferred velocity clamped per axis.
fn clamped(preferred: [i64; 2]) -> [i64; 2] {
    [preferred[0].clamp(-V, V), preferred[1].clamp(-V, V)]
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }
    /// Uniform-ish in `-span..=span`.
    fn signed(&mut self, span: i64) -> i64 {
        (self.next() % (2 * span as u64 + 1)) as i64 - span
    }
    /// An edge-heavy value in `-span..=span`.
    fn edge(&mut self, span: i64) -> i64 {
        match self.next() % 6 {
            0 => span,
            1 => -span,
            2 => span - 1,
            3 => 0,
            _ => self.signed(span),
        }
    }
}

// ---------------------------------------------------------------------------
// Checked reference: the kernel's arithmetic step by step with checked ops and
// checked narrowing. `None` means some intermediate would overflow its type.

fn ref_solve(me: &OrcaBody, preferred: [i64; 2], others: &[OrcaBody], s: &OrcaSettings) -> Option<[i64; 2]> {
    let mut order: Vec<(u128, u32, usize)> = Vec::new();
    for (i, o) in others.iter().enumerate() {
        if o.id == me.id {
            continue;
        }
        let dx = i128::from(o.x_mm).checked_sub(i128::from(me.x_mm))?;
        let dz = i128::from(o.z_mm).checked_sub(i128::from(me.z_mm))?;
        let d2 = dx.checked_mul(dx)?.checked_add(dz.checked_mul(dz)?)?;
        order.push((u128::try_from(d2).ok()?, o.id, i));
    }
    order.sort();
    order.truncate(s.max_neighbors);

    let mut pref = preferred;
    let p2 = pref[0]
        .checked_mul(pref[0])?
        .checked_add(pref[1].checked_mul(pref[1])?)?;
    let m2 = s.min_preferred_mm_s.checked_mul(s.min_preferred_mm_s)?;
    if p2 >= m2 && ref_head_on(me, &order, others, s)? {
        let wide = i128::from(pref[0])
            .checked_mul(i128::from(pref[0]))?
            .checked_add(i128::from(pref[1]).checked_mul(i128::from(pref[1]))?)?;
        let plen = i64::try_from(u64::try_from(wide).ok()?.isqrt()).ok()?;
        if plen > 0 {
            let bx = pref[1].checked_mul(s.keep_right_mm_s)?.checked_div(plen)?;
            let bz = pref[0]
                .checked_neg()?
                .checked_mul(s.keep_right_mm_s)?
                .checked_div(plen)?;
            pref[0] = pref[0].checked_add(bx)?;
            pref[1] = pref[1].checked_add(bz)?;
        }
    }

    let mut planes = Vec::new();
    for &(_, _, i) in &order {
        if let Some(plane) = ref_plane(me, &others[i], s)? {
            planes.push(plane);
        }
    }

    let mut v = pref;
    for (n, p) in &planes {
        let dot = v[0]
            .checked_sub(p[0])?
            .checked_mul(n[0])?
            .checked_add(v[1].checked_sub(p[1])?.checked_mul(n[1])?)?;
        if dot < 0 {
            let nn = n[0].checked_mul(n[0])?.checked_add(n[1].checked_mul(n[1])?)?;
            if nn == 0 {
                continue;
            }
            for axis in 0..2 {
                let step = i128::from(dot)
                    .checked_mul(i128::from(n[axis]))?
                    .checked_div(i128::from(nn))?;
                v[axis] = i64::try_from(i128::from(v[axis]).checked_sub(step)?).ok()?;
            }
        }
    }
    let smax2 = pref[0]
        .checked_mul(pref[0])?
        .checked_add(pref[1].checked_mul(pref[1])?)?;
    let v2 = v[0].checked_mul(v[0])?.checked_add(v[1].checked_mul(v[1])?)?;
    if v2 > smax2 {
        let len = i64::try_from(u64::try_from(v2).ok()?.isqrt()).ok()?;
        let max = i64::try_from(u64::try_from(smax2).ok()?.isqrt()).ok()?;
        if len == 0 {
            return Some([0, 0]);
        }
        v[0] = v[0].checked_mul(max)?.checked_div(len)?;
        v[1] = v[1].checked_mul(max)?.checked_div(len)?;
    }
    Some(v)
}

fn ref_head_on(
    me: &OrcaBody,
    order: &[(u128, u32, usize)],
    others: &[OrcaBody],
    s: &OrcaSettings,
) -> Option<bool> {
    let range = i128::from(s.head_on_range_mm);
    let range2 = range.checked_mul(range)?;
    for &(d2, _, i) in order {
        if i128::try_from(d2).ok()? > range2 {
            continue;
        }
        let o = &others[i];
        let px = i128::from(o.x_mm).checked_sub(i128::from(me.x_mm))?;
        let pz = i128::from(o.z_mm).checked_sub(i128::from(me.z_mm))?;
        let rr = i128::from(me.r_mm).checked_add(i128::from(o.r_mm))?;
        let wx = i128::from(me.vx_mm_s).checked_sub(i128::from(o.vx_mm_s))?;
        let wz = i128::from(me.vz_mm_s).checked_sub(i128::from(o.vz_mm_s))?;
        let ww = wx.checked_mul(wx)?.checked_add(wz.checked_mul(wz)?)?;
        if ww == 0 {
            continue;
        }
        if px.checked_mul(wx)?.checked_add(pz.checked_mul(wz)?)? <= 0 {
            continue;
        }
        let cross = px.checked_mul(wz)?.checked_sub(pz.checked_mul(wx)?)?;
        if cross.checked_mul(cross)? < rr.checked_mul(rr)?.checked_mul(ww)? {
            return Some(true);
        }
    }
    Some(false)
}

type Plane = ([i64; 2], [i64; 2]);

fn unit(a: i128, b: i128, len: i128) -> Option<[i64; 2]> {
    Some([
        i64::try_from(a.checked_mul(1000)?.checked_div(len)?).ok()?,
        i64::try_from(b.checked_mul(1000)?.checked_div(len)?).ok()?,
    ])
}

fn ref_plane(me: &OrcaBody, o: &OrcaBody, s: &OrcaSettings) -> Option<Option<Plane>> {
    let px = i128::from(o.x_mm).checked_sub(i128::from(me.x_mm))?;
    let pz = i128::from(o.z_mm).checked_sub(i128::from(me.z_mm))?;
    let rr = i128::from(me.r_mm).checked_add(i128::from(o.r_mm))?;
    let d2 = px.checked_mul(px)?.checked_add(pz.checked_mul(pz)?)?;
    let wx = i128::from(me.vx_mm_s).checked_sub(i128::from(o.vx_mm_s))?;
    let wz = i128::from(me.vz_mm_s).checked_sub(i128::from(o.vz_mm_s))?;
    if d2 < rr.checked_mul(rr)? {
        let d = i128::from(u64::try_from(d2).ok()?.isqrt());
        let towards = if d == 0 {
            if me.id < o.id { [1000, 0] } else { [-1000, 0] }
        } else {
            unit(px, pz, d)?
        };
        return Some(Some((
            [towards[0].checked_neg()?, towards[1].checked_neg()?],
            [o.vx_mm_s, o.vz_mm_s],
        )));
    }
    let ww = wx.checked_mul(wx)?.checked_add(wz.checked_mul(wz)?)?;
    if ww == 0 {
        return Some(None);
    }
    let closing = px.checked_mul(wx)?.checked_add(pz.checked_mul(wz)?)?;
    if closing <= 0 {
        return Some(None);
    }
    let t = closing.checked_mul(1000)?.checked_div(ww)?;
    if t <= 0 || t > i128::from(s.horizon_ms) {
        return Some(None);
    }
    let cx = px.checked_sub(wx.checked_mul(t)?.checked_div(1000)?)?;
    let cz = pz.checked_sub(wz.checked_mul(t)?.checked_div(1000)?)?;
    let c2 = cx.checked_mul(cx)?.checked_add(cz.checked_mul(cz)?)?;
    if c2 >= rr.checked_mul(rr)? {
        return Some(None);
    }
    let c = i128::from(u64::try_from(c2).ok()?.isqrt());
    let out = if c == 0 {
        if me.id < o.id { [1000, 0] } else { [-1000, 0] }
    } else {
        unit(cx, cz, c)?
    };
    let depth = rr.checked_sub(c)?;
    let rate = depth.checked_mul(1000)?.checked_div(t)?;
    let ux = i128::from(out[0])
        .checked_neg()?
        .checked_mul(rate)?
        .checked_div(1000)?;
    let uz = i128::from(out[1])
        .checked_neg()?
        .checked_mul(rate)?
        .checked_div(1000)?;
    let mu2 = ux.checked_mul(ux)?.checked_add(uz.checked_mul(uz)?)?;
    if mu2 == 0 {
        return Some(None);
    }
    let mu = i128::from(u64::try_from(mu2).ok()?.isqrt());
    let n = unit(ux, uz, mu)?;
    let point = [
        i64::try_from(i128::from(me.vx_mm_s).checked_add(ux / 2)?).ok()?,
        i64::try_from(i128::from(me.vz_mm_s).checked_add(uz / 2)?).ok()?,
    ];
    Some(Some((n, point)))
}

/// Settings at the edge of the domain.
fn edge_settings(max_neighbors: usize) -> OrcaSettings {
    OrcaSettings {
        max_neighbors,
        horizon_ms: ORCA_MAX_HORIZON_MS,
        keep_right_mm_s: V,
        min_preferred_mm_s: V,
        head_on_range_mm: ORCA_MAX_RANGE_MM,
        look_m: 20.0,
        ..OrcaSettings::default()
    }
}

// ---------------------------------------------------------------------------
// Acceptance: out-of-domain input.

#[test]
fn out_of_domain_input_is_the_same_in_debug_and_release() {
    let me = body(1, 0, 0, 5_000, 0, 2_000);
    let other = body(2, 10_000, 0, -5_000, 0, 2_000);
    let pref = [8_000, 0];
    let ok = OrcaSettings::default();
    type Case = (
        &'static str,
        OrcaBody,
        [i64; 2],
        Vec<OrcaBody>,
        OrcaSettings,
        OrcaDomainError,
    );
    let cap = ORCA_MAX_NEIGHBOR_PLANES;
    let crowd: Vec<OrcaBody> = (0..=cap as u32)
        .map(|k| body(10 + k, 3_000 + i64::from(k) * 10, 0, 0, 0, 1_000))
        .collect();
    let cases: Vec<Case> = vec![
        (
            "me x",
            body(1, P + 1, 0, 0, 0, 2_000),
            pref,
            vec![other],
            ok,
            OrcaDomainError::Position { id: 1 },
        ),
        (
            "me z",
            body(1, 0, -P - 1, 0, 0, 2_000),
            pref,
            vec![other],
            ok,
            OrcaDomainError::Position { id: 1 },
        ),
        (
            "other x",
            me,
            pref,
            vec![body(2, -P - 1, 0, 0, 0, 2_000)],
            ok,
            OrcaDomainError::Position { id: 2 },
        ),
        (
            "other z",
            me,
            pref,
            vec![body(2, 0, P + 1, 0, 0, 2_000)],
            ok,
            OrcaDomainError::Position { id: 2 },
        ),
        (
            "me vx",
            body(1, 0, 0, V + 1, 0, 2_000),
            pref,
            vec![other],
            ok,
            OrcaDomainError::Velocity { id: 1 },
        ),
        (
            "other vz",
            me,
            pref,
            vec![body(2, 10_000, 0, 0, -V - 1, 2_000)],
            ok,
            OrcaDomainError::Velocity { id: 2 },
        ),
        (
            "preferred x",
            me,
            [V + 1, 0],
            vec![other],
            ok,
            OrcaDomainError::Preferred,
        ),
        (
            "preferred z",
            me,
            [0, -V - 1],
            vec![other],
            ok,
            OrcaDomainError::Preferred,
        ),
        (
            "me radius",
            body(1, 0, 0, 0, 0, R + 1),
            pref,
            vec![other],
            ok,
            OrcaDomainError::Radius { id: 1 },
        ),
        (
            "other radius",
            me,
            pref,
            vec![body(2, 10_000, 0, 0, 0, R + 1)],
            ok,
            OrcaDomainError::Radius { id: 2 },
        ),
        (
            "negative radius",
            me,
            pref,
            vec![body(2, 10_000, 0, 0, 0, -1)],
            ok,
            OrcaDomainError::Radius { id: 2 },
        ),
        (
            "horizon",
            me,
            pref,
            vec![other],
            OrcaSettings {
                horizon_ms: ORCA_MAX_HORIZON_MS + 1,
                ..ok
            },
            OrcaDomainError::Horizon,
        ),
        (
            "negative horizon",
            me,
            pref,
            vec![other],
            OrcaSettings { horizon_ms: -1, ..ok },
            OrcaDomainError::Horizon,
        ),
        (
            "keep right",
            me,
            pref,
            vec![other],
            OrcaSettings {
                keep_right_mm_s: V + 1,
                ..ok
            },
            OrcaDomainError::KeepRight,
        ),
        (
            "min preferred",
            me,
            pref,
            vec![other],
            OrcaSettings {
                min_preferred_mm_s: V + 1,
                ..ok
            },
            OrcaDomainError::MinPreferred,
        ),
        (
            "head-on range",
            me,
            pref,
            vec![other],
            OrcaSettings {
                head_on_range_mm: ORCA_MAX_RANGE_MM + 1,
                ..ok
            },
            OrcaDomainError::HeadOnRange,
        ),
        (
            "neighbor planes",
            me,
            pref,
            crowd.clone(),
            OrcaSettings {
                max_neighbors: cap + 1,
                ..ok
            },
            OrcaDomainError::NeighborCap,
        ),
        // Far outside: these overflow i64 in the unchecked arithmetic.
        (
            "extreme preferred",
            me,
            [i64::MAX, i64::MIN],
            vec![other],
            ok,
            OrcaDomainError::Preferred,
        ),
        (
            "extreme velocity",
            body(1, 0, 0, 3 << 30, 0, 2_000),
            [3 << 30, 0],
            vec![body(2, 3_000, 0, -(3 << 30), 0, 2_000)],
            ok,
            OrcaDomainError::Velocity { id: 1 },
        ),
        (
            "extreme position",
            body(1, i64::MIN, i64::MAX, 0, 0, 2_000),
            pref,
            vec![other],
            ok,
            OrcaDomainError::Position { id: 1 },
        ),
    ];
    let mut scratch = OrcaScratch::new();
    for (name, me, preferred, others, settings, expected) in &cases {
        let input = OrcaInput {
            me,
            preferred: *preferred,
            others,
        };
        assert_eq!(input.validate(settings), Err(*expected), "{name}: validate");
        assert_eq!(
            try_orca_velocity(me, *preferred, others, settings),
            Err(*expected),
            "{name}: try_orca_velocity"
        );
        let documented = clamped(*preferred);
        assert_eq!(
            orca_velocity_with_settings(me, *preferred, others, settings),
            documented,
            "{name}: plain settings form"
        );
        assert_eq!(
            orca_velocity_into(&mut scratch, me, *preferred, others, settings),
            documented,
            "{name}: scratch form"
        );
        if *settings == ok {
            assert_eq!(
                orca_velocity(me, *preferred, others),
                documented,
                "{name}: defaults form"
            );
        }
        assert!(!expected.code().is_empty());
        assert!(
            expected
                .code()
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{name}: code {}",
            expected.code()
        );
    }
    assert_eq!(scratch.stats().out_of_domain, cases.len() as u64);

    // The edges themselves are in the domain.
    let edge_me = body(1, P, -P, V, -V, R);
    let edge_other = body(2, -P, P, -V, V, R);
    let input = OrcaInput {
        me: &edge_me,
        preferred: [V, -V],
        others: &[edge_other],
    };
    assert_eq!(input.validate(&edge_settings(cap)), Ok(()));
    let at_cap: Vec<_> = crowd[..cap].to_vec();
    let settings = OrcaSettings {
        max_neighbors: usize::MAX,
        ..ok
    };
    assert!(
        try_orca_velocity(&me, pref, &at_cap, &settings).is_ok(),
        "256 planes retained"
    );
    // A large cap is fine while the retained count stays within the plane bound.
    assert!(try_orca_velocity(&me, pref, &[other], &settings).is_ok());
    // Self entries never enter the arithmetic, so their values are not checked.
    let wild_self = body(1, i64::MAX, i64::MIN, i64::MAX, i64::MIN, -5);
    assert_eq!(
        try_orca_velocity(&me, pref, &[wild_self, other], &ok),
        Ok(orca_velocity(&me, pref, &[other]))
    );
    println!(
        "out_of_domain_input_is_the_same_in_debug_and_release: {} cases, debug_assertions={}",
        cases.len(),
        cfg!(debug_assertions)
    );
}

// ---------------------------------------------------------------------------
// Acceptance: the largest in-domain values through every arithmetic site.

fn check_case(me: &OrcaBody, pref: [i64; 2], others: &[OrcaBody], s: &OrcaSettings, what: &str) {
    let reference = ref_solve(me, pref, others, s);
    assert!(reference.is_some(), "{what}: checked reference overflowed");
    assert_eq!(
        try_orca_velocity(me, pref, others, s),
        Ok(reference.unwrap_or_default()),
        "{what}"
    );
}

#[test]
fn domain_edge_inputs_compute_without_overflow() {
    let cap = ORCA_MAX_NEIGHBOR_PLANES;
    // Opposite corners, maximal closing speed, maximal radii and settings.
    for (sx, sv) in [(1, 1), (1, -1), (-1, 1), (-1, -1)] {
        let me = body(1, sx * P, sx * P, sv * V, sv * V, R);
        let other = body(2, -sx * P, -sx * P, -sv * V, -sv * V, R);
        check_case(&me, [sv * V, sv * V], &[other], &edge_settings(cap), "corners");
        check_case(&me, [-V, V], &[other], &edge_settings(cap), "corners crossed");
    }
    // Overlap at the largest radii, including coincident centers.
    for dx in [0, 1, R, 2 * R - 1] {
        let me = body(1, P, P, V, -V, R);
        let other = body(2, P - dx, P, -V, V, R);
        check_case(&me, [V, V], &[other], &edge_settings(cap), "contact");
    }
    // Just outside contact with the fastest closing speed: the smallest horizon
    // time and the largest correction rate the domain admits.
    for gap in [1, 2, 7, 1_000, 50_000] {
        let me = body(1, 0, 0, V, 0, R);
        let other = body(2, 2 * R + gap, 1, -V, 0, R);
        check_case(&me, [V, 0], &[other], &edge_settings(cap), "fast approach");
        check_case(
            &me,
            [V, V],
            &[other],
            &edge_settings(1),
            "fast approach, one plane",
        );
    }
    // The plane bound: 256 retained neighbors, each a fast-approaching or overlapping
    // disc around the mover, with the head-on bias at its maximum.
    let mut rng = Lcg(0x5eed_0d0a_1a2b_3c4d);
    for round in 0..64 {
        let me = body(
            0,
            rng.edge(P - 400_000),
            rng.edge(P - 400_000),
            rng.edge(V),
            rng.edge(V),
            R,
        );
        let others: Vec<_> = (0..cap as u32)
            .map(|k| {
                let ang = rng.signed(2 * R + 200_000);
                body(
                    k + 1,
                    me.x_mm + ang,
                    me.z_mm + rng.signed(2 * R + 200_000),
                    rng.edge(V),
                    rng.edge(V),
                    rng.edge(R).abs(),
                )
            })
            .collect();
        let pref = [rng.edge(V), rng.edge(V)];
        check_case(&me, pref, &others, &edge_settings(cap), &format!("ring {round}"));
    }
    // Random edge-heavy sweep over every field.
    for case in 0..1_500 {
        let n = (rng.next() % 40) as u32;
        let near = rng.next().is_multiple_of(2);
        let me = body(
            0,
            rng.edge(P),
            rng.edge(P),
            rng.edge(V),
            rng.edge(V),
            rng.edge(R).abs(),
        );
        let others: Vec<_> = (0..n)
            .map(|k| {
                let (x, z) = if near {
                    let span = 4 * R;
                    (
                        (me.x_mm + rng.signed(span)).clamp(-P, P),
                        (me.z_mm + rng.signed(span)).clamp(-P, P),
                    )
                } else {
                    (rng.edge(P), rng.edge(P))
                };
                body(k + 1, x, z, rng.edge(V), rng.edge(V), rng.edge(R).abs())
            })
            .collect();
        let settings = OrcaSettings {
            max_neighbors: (rng.next() % (cap as u64 + 1)) as usize,
            horizon_ms: rng.edge(ORCA_MAX_HORIZON_MS).abs(),
            keep_right_mm_s: rng.edge(V).abs(),
            min_preferred_mm_s: rng.edge(V).abs(),
            head_on_range_mm: rng.edge(ORCA_MAX_RANGE_MM).abs(),
            look_m: 20.0,
            ..OrcaSettings::default()
        };
        let pref = [rng.edge(V), rng.edge(V)];
        check_case(&me, pref, &others, &settings, &format!("sweep {case}"));
    }
}

// ---------------------------------------------------------------------------
// Acceptance: the scratch form equals the pinned corpus and allocates nothing.

/// The 2048-case corpus of `orca_bits.rs` (pin generated from the unmodified
/// d6ed37677 source), here through the scratch form.
fn corpus() -> Vec<(OrcaBody, [i64; 2], Vec<OrcaBody>)> {
    let mut state = 0x2545_f491_u32;
    let mut next = |span: i64| {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        i64::from(state) % (span * 2 + 1) - span
    };
    let mut out = Vec::with_capacity(2048);
    for case in 0..2048 {
        let me = OrcaBody {
            id: 1,
            x_mm: next(1_000_000),
            z_mm: next(1_000_000),
            vx_mm_s: next(20_000),
            vz_mm_s: next(20_000),
            r_mm: 1 + next(2500).abs(),
        };
        let preferred = [next(20_000), next(20_000)];
        let others: Vec<_> = (0..case % 33)
            .map(|index| OrcaBody {
                id: index + 2,
                x_mm: me.x_mm + next(25_000),
                z_mm: me.z_mm + next(25_000),
                vx_mm_s: next(20_000),
                vz_mm_s: next(20_000),
                r_mm: 1 + next(2500).abs(),
            })
            .collect();
        out.push((me, preferred, others));
    }
    out
}

fn fold(hash: &mut u64, v: [i64; 2]) {
    for x in v {
        for byte in x.to_le_bytes() {
            *hash ^= u64::from(byte);
            *hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    }
}

#[test]
fn scratch_form_is_bit_identical_and_allocates_nothing() {
    let cases = corpus();
    let settings = OrcaSettings::default();
    let mut scratch = OrcaScratch::new();
    // Warm-up grows the two buffers to the corpus high-water mark.
    for (me, pref, others) in &cases {
        let _ = orca_velocity_into(&mut scratch, me, *pref, others, &settings);
    }
    scratch.reset_stats();
    let mut results = Vec::with_capacity(cases.len());
    let ((), allocs) = count_allocs(|| {
        for (me, pref, others) in &cases {
            results.push(orca_velocity_into(&mut scratch, me, *pref, others, &settings));
        }
    });
    assert_eq!(allocs, 0, "allocations after warm-up");
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for ((me, pref, others), got) in cases.iter().zip(&results) {
        assert_eq!(*got, orca_velocity(me, *pref, others));
        fold(&mut hash, *got);
    }
    assert_eq!(hash, 0xf1a1_97b3_f232_f005, "DO27 d6ed37677 corpus bits moved");
    let stats = scratch.stats();
    assert_eq!(stats.calls, 2048);
    assert_eq!(stats.out_of_domain, 0, "the corpus is inside the domain");
    assert!(stats.planes > 0 && stats.projections > 0 && stats.projections <= stats.planes);
    assert!(stats.neighbors >= stats.planes);
    println!("scratch corpus: {hash:#018x}, stats {stats:?}");
}

// ---------------------------------------------------------------------------
// Acceptance: neighbors from the spatial index equal the full list.

const AGENTS: usize = 900;
const TICKS: usize = 600;
const BOX_MM: i64 = 150_000;

fn spatial_rows(bodies: &[OrcaBody], rows: &mut Vec<SpatialRow>) {
    rows.clear();
    rows.extend(bodies.iter().enumerate().map(|(slot, b)| {
        let [x, z] = b.position_m();
        SpatialRow {
            slot,
            body: BroadBody::new([x, 0.0, z], 0.0, [0.0, 2.0]),
            group_mask: 1,
            flags: 0,
        }
    }));
}

fn look_filtered(bodies: &[OrcaBody], me: usize, look_mm: i64, out: &mut Vec<OrcaBody>) {
    out.clear();
    let m = bodies[me];
    let look2 = look_mm * look_mm;
    for b in bodies {
        let dx = b.x_mm - m.x_mm;
        let dz = b.z_mm - m.z_mm;
        if dx.abs() <= look_mm && dz.abs() <= look_mm && dx * dx + dz * dz <= look2 {
            out.push(*b);
        }
    }
}

#[test]
fn indexed_neighbors_equal_the_full_list() {
    let settings = OrcaSettings::default();
    let query = OrcaNeighborQuery::from_settings(&settings, u64::MAX);
    assert_eq!(query.look_mm, 20_000);
    let mut rng = Lcg(0x0900_0600_0f1a_197b);
    let mut bodies: Vec<OrcaBody> = (0..AGENTS as u32)
        .map(|id| {
            body(
                id + 100,
                rng.signed(BOX_MM),
                rng.signed(BOX_MM),
                rng.signed(4_000),
                rng.signed(4_000),
                400 + rng.signed(300).abs() * 4,
            )
        })
        .collect();
    let mut prefs: Vec<[i64; 2]> = (0..AGENTS)
        .map(|_| [rng.signed(6_000), rng.signed(6_000)])
        .collect();
    let mut index = DetSpatial::new(BroadphaseConfig {
        cell_size: 10.0,
        ..Default::default()
    });
    let mut rows = Vec::new();
    let mut scratch = OrcaScratch::new();
    let mut full_scratch = OrcaScratch::new();
    let mut near = Vec::new();
    let mut next = vec![[0_i64; 2]; AGENTS];
    let (mut full_cap, mut short_list, mut compared_all) = (0_u64, 0_u64, 0_u64);
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for tick in 0..TICKS {
        spatial_rows(&bodies, &mut rows);
        assert!(index.build(&rows).is_ok());
        for (i, out) in next.iter_mut().enumerate() {
            let got =
                orca_velocity_indexed(&mut scratch, &mut index, &bodies, i, prefs[i], &settings, &query);
            look_filtered(&bodies, i, query.look_mm, &mut near);
            let full = orca_velocity_into(&mut full_scratch, &bodies[i], prefs[i], &near, &settings);
            assert_eq!(got, full, "tick {tick} agent {i}: look-filtered full list");
            // `near` includes the mover itself; the kernel drops its own ID.
            let in_look = near.len() - 1;
            if in_look >= settings.max_neighbors {
                full_cap += 1;
            } else {
                short_list += 1;
            }
            if tick.is_multiple_of(60) && in_look >= settings.max_neighbors {
                // Proof obligation: with the cap filled inside the look distance the
                // retained set equals the unfiltered list's, so the answer does too.
                let all = orca_velocity_into(&mut full_scratch, &bodies[i], prefs[i], &bodies, &settings);
                assert_eq!(got, all, "tick {tick} agent {i}: unfiltered full list");
                compared_all += 1;
            }
            *out = got;
        }
        // Snapshot semantics: every agent solved against the same tick, then move.
        for (i, b) in bodies.iter_mut().enumerate() {
            let v = next[i];
            b.vx_mm_s = v[0];
            b.vz_mm_s = v[1];
            b.x_mm += v[0] / 10;
            b.z_mm += v[1] / 10;
            if b.x_mm.abs() > BOX_MM {
                prefs[i][0] = -prefs[i][0];
                b.x_mm = b.x_mm.clamp(-BOX_MM, BOX_MM);
            }
            if b.z_mm.abs() > BOX_MM {
                prefs[i][1] = -prefs[i][1];
                b.z_mm = b.z_mm.clamp(-BOX_MM, BOX_MM);
            }
            fold(&mut hash, [b.x_mm, b.z_mm]);
        }
    }
    let stats = scratch.stats();
    println!(
        "indexed crowd: {AGENTS} agents x {TICKS} ticks, cap filled {full_cap}, short lists {short_list}, \
         unfiltered comparisons {compared_all}, state hash {hash:#018x}, stats {stats:?}"
    );
    assert!(
        full_cap > 0 && short_list > 0,
        "both sides of the look rule exercised"
    );
    assert!(compared_all > 0);
    assert_eq!(stats.out_of_domain, 0);
    assert_eq!(stats.calls, (AGENTS * TICKS) as u64);
}

#[test]
fn indexed_look_boundary_is_exact_at_far_positions() {
    let settings = OrcaSettings::default();
    let query = OrcaNeighborQuery::from_settings(&settings, u64::MAX);
    let look = query.look_mm;
    for &(cx, cz) in &[
        (0, 0),
        (P - look - 5, P - look - 5),
        (-P + look + 5, 12_345),
        (P - 1, -P + 1),
    ] {
        let me = body(1, cx, cz, 0, 0, 500);
        // Exactly at the look distance (in), one millimeter beyond (out), and
        // a diagonal pair on either side of the exact integer circle.
        let candidates = [
            body(2, cx - look, cz, 0, 0, 500),
            body(3, cx, cz + look, 0, 0, 500),
            body(4, cx + look + 1, cz, 0, 0, 500),
            body(5, cx + 12_000, cz + 16_000, 0, 0, 500),
            body(6, cx - 12_000, cz - 16_001, 0, 0, 500),
        ];
        let mut bodies = vec![me];
        bodies.extend(
            candidates
                .iter()
                .filter(|b| (-P..=P).contains(&b.x_mm) && (-P..=P).contains(&b.z_mm)),
        );
        let mut rows = Vec::new();
        spatial_rows(&bodies, &mut rows);
        let mut index = DetSpatial::default();
        assert!(index.build(&rows).is_ok());
        let mut scratch = OrcaScratch::new();
        let mut near = Vec::new();
        look_filtered(&bodies, 0, look, &mut near);
        let expected_ids: Vec<u32> = near.iter().filter(|b| b.id != 1).map(|b| b.id).collect();
        for b in &near {
            assert!(b.id != 4 && b.id != 6, "integer filter at ({cx}, {cz})");
        }
        // All in-look neighbors are retained (fewer than the cap), so the kernel's
        // neighbor counter shows the gathered set had the expected size.
        let got = try_orca_velocity_indexed(
            &mut scratch,
            &mut index,
            &bodies,
            0,
            [1_000, 0],
            &settings,
            &query,
        );
        let full = try_orca_velocity(&me, [1_000, 0], &near, &settings);
        assert_eq!(got, full, "at ({cx}, {cz})");
        assert_eq!(
            scratch.stats().neighbors,
            expected_ids.len() as u64,
            "at ({cx}, {cz})"
        );
    }
}

#[test]
fn indexed_form_reports_bad_slots_and_allocates_nothing() {
    let settings = OrcaSettings::default();
    let query = OrcaNeighborQuery::from_settings(&settings, u64::MAX);
    let bodies: Vec<_> = (0..64)
        .map(|k| {
            body(
                k + 1,
                i64::from(k % 8) * 3_000,
                i64::from(k / 8) * 3_000,
                1_000,
                0,
                1_000,
            )
        })
        .collect();
    let mut rows = Vec::new();
    spatial_rows(&bodies, &mut rows);
    let mut index = DetSpatial::default();
    assert!(index.build(&rows).is_ok());
    let mut scratch = OrcaScratch::new();
    assert_eq!(
        try_orca_velocity_indexed(
            &mut scratch,
            &mut index,
            &bodies,
            64,
            [1_000, 0],
            &settings,
            &query
        ),
        Err(OrcaDomainError::Slot)
    );
    assert_eq!(
        orca_velocity_indexed(
            &mut scratch,
            &mut index,
            &bodies,
            64,
            [V + 9, 0],
            &settings,
            &query
        ),
        [V, 0]
    );
    // An index with more rows than bodies is a host error, never a panic.
    assert_eq!(
        try_orca_velocity_indexed(
            &mut scratch,
            &mut index,
            &bodies[..10],
            0,
            [1_000, 0],
            &settings,
            &query
        ),
        Err(OrcaDomainError::Slot)
    );
    let bad_look = OrcaNeighborQuery {
        look_mm: ORCA_MAX_RANGE_MM + 1,
        mask: u64::MAX,
    };
    assert_eq!(
        try_orca_velocity_indexed(
            &mut scratch,
            &mut index,
            &bodies,
            0,
            [1_000, 0],
            &settings,
            &bad_look
        ),
        Err(OrcaDomainError::Look)
    );
    // Warm, then count.
    for i in 0..bodies.len() {
        let _ = orca_velocity_indexed(
            &mut scratch,
            &mut index,
            &bodies,
            i,
            [1_000, 0],
            &settings,
            &query,
        );
    }
    let ((), allocs) = count_allocs(|| {
        for i in 0..bodies.len() {
            let _ = orca_velocity_indexed(
                &mut scratch,
                &mut index,
                &bodies,
                i,
                [1_000, 0],
                &settings,
                &query,
            );
        }
    });
    assert_eq!(allocs, 0);
    // The mask is part of the neighbor rule: a mask that matches no row leaves
    // only the preferred velocity (no neighbors).
    let none = OrcaNeighborQuery {
        look_mm: 20_000,
        mask: 0,
    };
    assert_eq!(
        orca_velocity_indexed(&mut scratch, &mut index, &bodies, 9, [1_000, 0], &settings, &none),
        orca_velocity(&bodies[9], [1_000, 0], &[])
    );
}

// ---------------------------------------------------------------------------
// Evidence only (no assertions on time): per-tick cost of a 900-agent crowd.
// `cargo test --release -p regia_detsim --test orca_domain -- --ignored --nocapture`

#[test]
#[ignore = "timing evidence; run in --release on an idle host"]
#[expect(
    clippy::disallowed_methods,
    reason = "Instant::now in the #[ignore]d timing probe: prints costs, never steps a sim (ADR-8367)"
)]
fn crowd_900_tick_timing_evidence() {
    use std::time::Instant;
    let settings = OrcaSettings::default();
    let query = OrcaNeighborQuery::from_settings(&settings, u64::MAX);
    let mut rng = Lcg(0x0900_0600_0f1a_197b);
    let mut bodies: Vec<OrcaBody> = (0..AGENTS as u32)
        .map(|id| {
            body(
                id + 100,
                rng.signed(BOX_MM),
                rng.signed(BOX_MM),
                rng.signed(4_000),
                rng.signed(4_000),
                400 + rng.signed(300).abs() * 4,
            )
        })
        .collect();
    let prefs: Vec<[i64; 2]> = (0..AGENTS)
        .map(|_| [rng.signed(6_000), rng.signed(6_000)])
        .collect();
    let mut index = DetSpatial::new(BroadphaseConfig {
        cell_size: 10.0,
        ..Default::default()
    });
    let mut rows = Vec::new();
    let mut scratch = OrcaScratch::new();
    let mut lists: Vec<Vec<OrcaBody>> = vec![Vec::new(); AGENTS];
    let mut out = vec![[0_i64; 2]; AGENTS];
    let (mut t_alloc, mut t_scratch, mut t_index, mut t_build) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut t_lists = Vec::new();
    for _tick in 0..TICKS {
        // Host-built look lists by brute force (the pre-index host path).
        let t0 = Instant::now();
        for (i, list) in lists.iter_mut().enumerate() {
            look_filtered(&bodies, i, query.look_mm, list);
        }
        t_lists.push(t0.elapsed().as_secs_f64() * 1e3);
        let t0 = Instant::now();
        for i in 0..AGENTS {
            out[i] = orca_velocity_with_settings(&bodies[i], prefs[i], &lists[i], &settings);
        }
        t_alloc.push(t0.elapsed().as_secs_f64() * 1e3);
        let t0 = Instant::now();
        for i in 0..AGENTS {
            out[i] = orca_velocity_into(&mut scratch, &bodies[i], prefs[i], &lists[i], &settings);
        }
        t_scratch.push(t0.elapsed().as_secs_f64() * 1e3);
        let t0 = Instant::now();
        spatial_rows(&bodies, &mut rows);
        let _ = index.build(&rows);
        t_build.push(t0.elapsed().as_secs_f64() * 1e3);
        let t0 = Instant::now();
        for (i, o) in out.iter_mut().enumerate() {
            *o = orca_velocity_indexed(&mut scratch, &mut index, &bodies, i, prefs[i], &settings, &query);
        }
        t_index.push(t0.elapsed().as_secs_f64() * 1e3);
        for (b, v) in bodies.iter_mut().zip(&out) {
            b.vx_mm_s = v[0];
            b.vz_mm_s = v[1];
            b.x_mm = (b.x_mm + v[0] / 10).clamp(-BOX_MM, BOX_MM);
            b.z_mm = (b.z_mm + v[1] / 10).clamp(-BOX_MM, BOX_MM);
        }
    }
    let pct = |v: &mut Vec<f64>, q: f64| {
        v.sort_by(f64::total_cmp);
        v[((v.len() - 1) as f64 * q) as usize]
    };
    for (name, v) in [
        ("host brute-force look lists (900 x 900)", &mut t_lists),
        ("per-call Vec form (host look lists)", &mut t_alloc),
        ("scratch form (host look lists)", &mut t_scratch),
        ("index build (900 rows)", &mut t_build),
        ("indexed form (queries + solve)", &mut t_index),
    ] {
        println!(
            "{name}: p50 {:.4} ms, p99 {:.4} ms, max {:.4} ms per tick",
            pct(v, 0.5),
            pct(v, 0.99),
            pct(v, 1.0)
        );
    }
}
