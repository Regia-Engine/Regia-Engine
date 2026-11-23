// SPDX-License-Identifier: MPL-2.0
//! Agent methods for mesh debris bursts and impact sites (`book/agents/debris-impact.md`):
//! `vfx.debris_burst`, `vfx.debris_state`, `vfx.debris_clock`, `vfx.impact_site`.
//!
//! They need `regia_vfx::DebrisPlugin` (and `ImpactSitePlugin` for sites). Without the
//! resources every call returns `UNSUPPORTED` (`F0016`). Debris poses are a pure function of the
//! [`regia_vfx::DebrisClock`] frame, so `vfx.debris_state` returns a pose hash an agent can
//! compare across runs.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::QualityTier;
use regia_vfx::debris_mesh::{DebrisClass, DebrisSpec, MAX_FRAGMENTS};
use regia_vfx::{DebrisBursts, DebrisClock, DebrisMeshLibrary, ImpactDetail, ImpactSiteSpec, ImpactSites};
use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};

/// Largest coordinate magnitude accepted (metres), the same bound the contract declares.
const MAX_COORD: f64 = 1_000_000.0;

fn unsupported(what: &str, hint: &str) -> RpcError {
    RpcError::new(protocol::UNSUPPORTED, "F0016", what, hint)
}

fn require_debris(world: &World) -> Result<(), RpcError> {
    if world.get_resource::<DebrisBursts>().is_none() || world.get_resource::<DebrisClock>().is_none() {
        return Err(unsupported(
            "no debris system in this app",
            "add regia_vfx::DebrisPlugin so vfx.debris_burst has bursts and a clock",
        ));
    }
    Ok(())
}

fn require_sites(world: &World) -> Result<(), RpcError> {
    if world.get_resource::<ImpactSites>().is_none() || world.get_resource::<DebrisClock>().is_none() {
        return Err(unsupported(
            "no impact sites in this app",
            "add regia_vfx::ImpactSitePlugin (and DebrisPlugin for the clock) so vfx.impact_site has sites",
        ));
    }
    Ok(())
}

fn number(params: &Value, field: &str, default: f64, lo: f64, hi: f64) -> Result<f64, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => {
            let n = value
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number")))?;
            if !n.is_finite() || n < lo || n > hi {
                return Err(RpcError::invalid_params(format!(
                    "`{field}` must be finite and in {lo}..={hi}"
                )));
            }
            Ok(n)
        }
    }
}

fn unsigned(params: &Value, field: &str) -> Result<Option<u64>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a non-negative integer"))),
    }
}

fn point(params: &Value, field: &str) -> Result<Vec3, RpcError> {
    let invalid = || RpcError::invalid_params(format!("`{field}` ([x, y, z] in metres) is required"));
    let items = params.get(field).and_then(Value::as_array).ok_or_else(invalid)?;
    if items.len() != 3 {
        return Err(invalid());
    }
    let mut out = [0.0f32; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        let n = item.as_f64().ok_or_else(invalid)?;
        if !n.is_finite() || n.abs() > MAX_COORD {
            return Err(RpcError::invalid_params(format!(
                "`{field}` must be finite and within {MAX_COORD} m"
            )));
        }
        *slot = n as f32;
    }
    Ok(Vec3::from(out))
}

fn tier_param(params: &Value) -> Result<QualityTier, RpcError> {
    match params.get("tier") {
        None | Some(Value::Null) => Ok(QualityTier::High),
        Some(Value::String(name)) => QualityTier::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "`tier` {name}: MobileLow, MobileHigh, Deck, Low, Medium, High or Ultra"
            ))
        }),
        Some(_) => Err(RpcError::invalid_params(
            "`tier` must be a string: MobileLow, MobileHigh, Deck, Low, Medium, High or Ultra",
        )),
    }
}

fn clock_json(clock: &DebrisClock) -> Value {
    json!({ "frame": clock.frame, "auto": clock.auto })
}

/// `vfx.debris_burst`: create a burst of mesh debris, or remove one with `remove`.
pub fn debris_burst(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_debris(world)?;
    if let Some(id) = unsigned(params, "remove")? {
        let removed = world.resource_mut::<DebrisBursts>().remove(id);
        if !removed {
            return Err(RpcError::invalid_params(format!("no debris burst {id}")));
        }
        return Ok(json!({ "removed": id }));
    }
    let origin = point(params, "origin")?;
    let seed = unsigned(params, "seed")?.unwrap_or(1);
    let frame_now = world.resource::<DebrisClock>().frame;
    let start_frame = unsigned(params, "start_frame")?.unwrap_or(frame_now);
    let tier = tier_param(params)?;
    let ground_y = number(params, "ground_y", f64::from(origin.y), -MAX_COORD, MAX_COORD)? as f32;
    let gravity = number(params, "gravity", 9.81, 0.1, 100.0)? as f32;
    let fps = number(params, "fps", 60.0, 1.0, 1000.0)? as f32;
    let mut spec = DebrisSpec::earth_blast(origin, seed, start_frame);
    spec.gravity = gravity;
    spec.fps = fps;
    if let Some(counts) = params.get("counts") {
        let counts = counts
            .as_object()
            .ok_or_else(|| RpcError::invalid_params("`counts` must be an object of class -> count"))?;
        for (name, value) in counts {
            let class = DebrisClass::from_name(name).ok_or_else(|| {
                RpcError::invalid_params(format!("`counts` {name}: earth, stone, turf or hardware"))
            })?;
            let n = value
                .as_u64()
                .filter(|n| *n <= u64::from(MAX_FRAGMENTS))
                .ok_or_else(|| {
                    RpcError::invalid_params(format!("`counts.{name}` must be 0..={MAX_FRAGMENTS}"))
                })?;
            if let Some(c) = spec.classes.iter_mut().find(|c| c.class == class) {
                c.count = n as u32;
            }
        }
    }
    spec.apply_tier(tier);
    let ground = move |_x: f32, _z: f32| ground_y;
    let id = world
        .resource_mut::<DebrisBursts>()
        .spawn(spec.clone(), &ground)
        .map_err(|err| RpcError::invalid_params(err.to_string()))?;
    let meshes = world
        .get_resource::<DebrisMeshLibrary>()
        .map(DebrisMeshLibrary::counts);
    let mut classes = Map::new();
    for c in &spec.classes {
        classes.insert(c.class.name().into(), json!(c.count));
    }
    Ok(json!({
        "burst": id,
        "fragments": spec.total(),
        "classes": classes,
        "start_frame": start_frame,
        "tier": tier.name(),
        "cheap": spec.cheap,
        "meshes": meshes.map(|m| m.iter().map(|(c, n)| (c.name().to_string(), json!(n))).collect::<Map<_, _>>()),
    }))
}

/// `vfx.debris_state`: the clock, the mesh library, every burst (live entities, landed count and
/// the pose hash at `frame`, default the clock's) and every impact site.
pub fn debris_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_debris(world)?;
    let clock = *world.resource::<DebrisClock>();
    let frame = unsigned(params, "frame")?.unwrap_or(clock.frame);
    let bursts: Vec<Value> = world
        .resource::<DebrisBursts>()
        .info(frame)
        .into_iter()
        .map(|b| {
            json!({
                "burst": b.id,
                "start_frame": b.start_frame,
                "fragments": b.fragments,
                "live": b.live,
                "landed": b.landed,
                "hash": format!("{:016x}", b.hash),
            })
        })
        .collect();
    let meshes: Map<String, Value> = world
        .get_resource::<DebrisMeshLibrary>()
        .map(|lib| {
            lib.counts()
                .iter()
                .map(|(c, n)| (c.name().to_string(), json!(n)))
                .collect()
        })
        .unwrap_or_default();
    let sites: Vec<Value> = world
        .get_resource::<ImpactSites>()
        .map(|s| {
            s.sites()
                .iter()
                .map(|site| {
                    json!({
                        "site": site.id,
                        "at_frame": site.at_frame,
                        "center": [site.spec.center.x, site.spec.center.y, site.spec.center.z],
                        "detail": site.detail.name(),
                        "placed": site.is_placed(),
                        "puffed": site.puffed(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "clock": clock_json(&clock),
        "frame": frame,
        "meshes": meshes,
        "bursts": bursts,
        "sites": sites,
    }))
}

/// `vfx.debris_clock`: read or set the frame the debris and impact sites are posed for.
pub fn debris_clock(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_debris(world)?;
    let frame = unsigned(params, "frame")?;
    let auto = match params.get("auto") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`auto` must be a boolean"))?,
        ),
    };
    let before = clock_json(world.resource::<DebrisClock>());
    {
        let mut clock = world.resource_mut::<DebrisClock>();
        if let Some(frame) = frame {
            clock.frame = frame;
        }
        if let Some(auto) = auto {
            clock.auto = auto;
        }
    }
    Ok(json!({ "before": before, "after": clock_json(world.resource::<DebrisClock>()) }))
}

/// `vfx.impact_site`: schedule a crater, burnt ground and ejecta blanket at a frame (and the
/// first-contact puff just before it), or remove one with `remove`.
pub fn impact_site(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_sites(world)?;
    if let Some(id) = unsigned(params, "remove")? {
        let removed = world.resource_mut::<ImpactSites>().remove(id);
        if !removed {
            return Err(RpcError::invalid_params(format!("no impact site {id}")));
        }
        return Ok(json!({ "removed": id }));
    }
    let defaults = ImpactSiteSpec::default();
    let spec = ImpactSiteSpec {
        center: point(params, "center")?,
        radius: number(params, "radius", f64::from(defaults.radius), 1.0, 5_000.0)? as f32,
        depth: number(params, "depth", f64::from(defaults.depth), 0.1, 2_000.0)? as f32,
        rim_height: number(params, "rim_height", f64::from(defaults.rim_height), 0.0, 500.0)? as f32,
        blanket_radius: number(
            params,
            "blanket_radius",
            f64::from(defaults.blanket_radius),
            1.0,
            20_000.0,
        )? as f32,
        scorch_radius: number(
            params,
            "scorch_radius",
            f64::from(defaults.scorch_radius),
            1.0,
            50_000.0,
        )? as f32,
        seed: unsigned(params, "seed")?.unwrap_or(defaults.seed),
    };
    let frame_now = world.resource::<DebrisClock>().frame;
    let at_frame = unsigned(params, "at_frame")?.unwrap_or(frame_now);
    let puff_lead =
        unsigned(params, "puff_lead")?.unwrap_or(regia_vfx::impact_site::DEFAULT_PUFF_LEAD_FRAMES);
    let detail = match params.get("detail") {
        None | Some(Value::Null) => ImpactDetail::for_tier(tier_param(params)?),
        Some(Value::String(name)) => ImpactDetail::from_name(name)
            .ok_or_else(|| RpcError::invalid_params(format!("`detail` {name}: minimal, reduced or full")))?,
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`detail` must be a string: minimal, reduced or full",
            ));
        }
    };
    let id = world
        .resource_mut::<ImpactSites>()
        .schedule(spec, at_frame, puff_lead, detail)
        .map_err(|err| RpcError::invalid_params(err.to_string()))?;
    Ok(json!({
        "site": id,
        "at_frame": at_frame,
        "puff_frame": at_frame.saturating_sub(puff_lead),
        "detail": detail.name(),
        "decals": spec.decals(detail).len(),
        "floor_y": spec.floor_y(),
        "rim": {
            "height": spec.height_delta(spec.radius),
            "radius": spec.radius,
        },
        "blanket_radius": spec.blanket_radius,
        "scorch_radius": spec.scorch_radius,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_world() -> World {
        let mut world = World::new();
        world.init_resource::<DebrisBursts>();
        world.init_resource::<DebrisClock>();
        world.init_resource::<ImpactSites>();
        world
    }

    fn msg(err: &RpcError) -> String {
        err.message.clone()
    }

    #[test]
    fn every_method_reports_unsupported_without_its_resources() {
        let mut world = World::new();
        for result in [
            debris_burst(&mut world, &json!({ "origin": [0, 0, 0] })),
            debris_state(&mut world, &json!({})),
            debris_clock(&mut world, &json!({})),
            impact_site(&mut world, &json!({ "center": [0, 0, 0] })),
        ] {
            let err = result.expect_err("unsupported");
            assert_eq!(err.code, protocol::UNSUPPORTED);
        }
        // Bursts without a clock are still unsupported, and sites need the clock too.
        world.init_resource::<DebrisBursts>();
        assert!(debris_burst(&mut world, &json!({ "origin": [0, 0, 0] })).is_err());
        world.init_resource::<ImpactSites>();
        assert!(impact_site(&mut world, &json!({ "center": [0, 0, 0] })).is_err());
    }

    #[test]
    fn origin_is_required_and_bounded() {
        let mut world = app_world();
        for origin in [
            json!(null),
            json!([0, 0]),
            json!([0, 0, 0, 0]),
            json!([0, 0, "z"]),
            json!([0, 0, 1_000_001]),
            json!([-1_000_001, 0, 0]),
            json!([1.0e300, 0, 0]),
            json!("here"),
        ] {
            let err = debris_burst(&mut world, &json!({ "origin": origin })).expect_err("origin");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{origin}");
        }
        let err = debris_burst(&mut world, &json!({})).expect_err("missing origin");
        assert!(msg(&err).contains("origin"), "{}", msg(&err));
        // The boundary itself is allowed.
        debris_burst(
            &mut world,
            &json!({ "origin": [1_000_000, -1_000_000, 0], "counts": { "earth": 1 } }),
        )
        .expect("boundary");
    }

    #[test]
    fn numeric_options_are_range_checked_at_both_ends() {
        let mut world = app_world();
        let base = |k: &str, v: Value| json!({ "origin": [0, 0, 0], k: v });
        for (k, v) in [
            ("gravity", json!(0.05)),
            ("gravity", json!(100.5)),
            ("gravity", json!("g")),
            ("fps", json!(0.5)),
            ("fps", json!(1001)),
            ("ground_y", json!(2_000_000)),
            ("seed", json!(-1)),
            ("seed", json!(1.5)),
            ("start_frame", json!("now")),
        ] {
            let err = debris_burst(&mut world, &base(k, v.clone())).expect_err(k);
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{k}={v}");
        }
        assert_eq!(
            world.resource::<DebrisBursts>().info(0).len(),
            0,
            "a refused call spawns nothing"
        );
        for (k, v) in [
            ("gravity", json!(0.1)),
            ("gravity", json!(100)),
            ("fps", json!(1)),
            ("fps", json!(1000)),
        ] {
            let mut w = base(k, v);
            w["counts"] = json!({ "earth": 1 });
            debris_burst(&mut world, &w).expect("boundary value");
        }
    }

    #[test]
    fn tier_and_detail_must_be_strings_not_silently_defaulted() {
        let mut world = app_world();
        let err =
            debris_burst(&mut world, &json!({ "origin": [0, 0, 0], "tier": 5 })).expect_err("tier type");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        let err = debris_burst(&mut world, &json!({ "origin": [0, 0, 0], "tier": "Gigantic" }))
            .expect_err("tier name");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        let err =
            impact_site(&mut world, &json!({ "center": [0, 0, 0], "detail": 3 })).expect_err("detail type");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        let err = impact_site(&mut world, &json!({ "center": [0, 0, 0], "detail": "huge" }))
            .expect_err("detail name");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        let err =
            impact_site(&mut world, &json!({ "center": [0, 0, 0], "tier": [] })).expect_err("tier type");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        // null means absent.
        impact_site(
            &mut world,
            &json!({ "center": [0, 0, 0], "tier": null, "detail": null }),
        )
        .expect("nulls");
    }

    #[test]
    fn counts_are_validated_and_an_all_zero_burst_is_refused() {
        let mut world = app_world();
        let mut at =
            |counts: Value| debris_burst(&mut world, &json!({ "origin": [0, 0, 0], "counts": counts }));
        assert!(at(json!([1])).is_err(), "counts must be an object");
        assert!(at(json!({ "gravel": 3 })).is_err(), "unknown class");
        assert!(at(json!({ "earth": -1 })).is_err());
        assert!(at(json!({ "earth": 8193 })).is_err(), "over MAX_FRAGMENTS");
        assert!(at(json!({ "earth": 1.5 })).is_err());
        assert!(
            at(json!({ "earth": 0, "stone": 0, "turf": 0, "hardware": 0 })).is_err(),
            "an empty burst is a refusal"
        );
        let out = at(json!({ "earth": 8192, "stone": 0, "turf": 0, "hardware": 0, })).expect("max count");
        assert_eq!(
            out["classes"]["earth"],
            json!(8192.min(out["fragments"].as_u64().unwrap_or(0)))
        );
    }

    #[test]
    fn burst_cap_remove_and_state_round_trip() {
        let mut world = app_world();
        let small = json!({ "origin": [0, 5, 0], "counts": { "earth": 4, "stone": 0, "turf": 0, "hardware": 0 }, "seed": 9 });
        let mut ids = Vec::new();
        for _ in 0..regia_vfx::debris_mesh::MAX_BURSTS {
            ids.push(
                debris_burst(&mut world, &small).expect("burst")["burst"]
                    .as_u64()
                    .expect("id"),
            );
        }
        let err = debris_burst(&mut world, &small).expect_err("cap");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        assert!(msg(&err).contains("already alive"), "{}", msg(&err));
        let first = ids[0];
        assert_eq!(
            debris_burst(&mut world, &json!({ "remove": first })).expect("remove")["removed"],
            json!(first)
        );
        let err = debris_burst(&mut world, &json!({ "remove": first })).expect_err("second remove");
        assert!(msg(&err).contains("no debris burst"), "{}", msg(&err));
        let err = debris_burst(&mut world, &json!({ "remove": "x" })).expect_err("bad id");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        // A freed slot can be reused, and ids are never reused.
        let again = debris_burst(&mut world, &small).expect("slot freed")["burst"]
            .as_u64()
            .expect("id");
        assert!(!ids.contains(&again));
        let state = debris_state(&mut world, &json!({})).expect("state");
        assert_eq!(
            state["bursts"].as_array().map(Vec::len),
            Some(regia_vfx::debris_mesh::MAX_BURSTS)
        );
    }

    #[test]
    fn identical_bursts_have_identical_pose_hashes_and_the_hash_follows_the_frame() {
        let mut world = app_world();
        let spec = json!({ "origin": [3, 1, -2], "counts": { "earth": 40, "stone": 10, "turf": 0, "hardware": 2 }, "seed": 77, "start_frame": 0 });
        let a = debris_burst(&mut world, &spec).expect("a")["burst"]
            .as_u64()
            .expect("id");
        let b = debris_burst(&mut world, &spec).expect("b")["burst"]
            .as_u64()
            .expect("id");
        let at = |world: &mut World, frame: u64| {
            let s = debris_state(world, &json!({ "frame": frame })).expect("state");
            let hash = |id: u64| {
                s["bursts"]
                    .as_array()
                    .and_then(|v| v.iter().find(|x| x["burst"] == json!(id)))
                    .and_then(|x| x["hash"].as_str().map(str::to_string))
                    .unwrap_or_default()
            };
            (hash(a), hash(b))
        };
        let (a30, b30) = at(&mut world, 30);
        assert_eq!(a30, b30, "same spec, same frame, same poses");
        assert_eq!(a30.len(), 16);
        assert_eq!(
            at(&mut world, 30),
            (a30.clone(), b30),
            "repeated reads are stable"
        );
        let (a90, _) = at(&mut world, 90);
        assert_ne!(a30, a90, "poses move with the frame");
        // A different seed differs.
        let other = debris_burst(&mut world, &json!({ "origin": [3, 1, -2], "counts": { "earth": 40, "stone": 10, "turf": 0, "hardware": 2 }, "seed": 78, "start_frame": 0 }))
            .expect("other")["burst"].as_u64().expect("id");
        let s = debris_state(&mut world, &json!({ "frame": 30 })).expect("state");
        let hash = s["bursts"]
            .as_array()
            .and_then(|v| v.iter().find(|x| x["burst"] == json!(other)))
            .map(|x| x["hash"].clone());
        assert_ne!(hash, Some(json!(a30)));
    }

    #[test]
    fn the_clock_reads_sets_and_rejects_bad_types_without_partial_writes() {
        let mut world = app_world();
        let out = debris_clock(&mut world, &json!({})).expect("read");
        assert_eq!(out["before"], out["after"]);
        let out = debris_clock(&mut world, &json!({ "frame": 120, "auto": false })).expect("set");
        assert_eq!(out["before"], json!({ "frame": 0, "auto": true }));
        assert_eq!(out["after"], json!({ "frame": 120, "auto": false }));
        // A bad `auto` refuses the whole call: the valid `frame` is not applied.
        let err = debris_clock(&mut world, &json!({ "frame": 5, "auto": "yes" })).expect_err("auto");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        assert_eq!(world.resource::<DebrisClock>().frame, 120);
        assert!(debris_clock(&mut world, &json!({ "frame": -1 })).is_err());
        assert!(debris_clock(&mut world, &json!({ "frame": 1.5 })).is_err());
        // The largest frame is accepted and state still reads at it.
        debris_clock(&mut world, &json!({ "frame": u64::MAX })).expect("max frame");
        debris_state(&mut world, &json!({})).expect("state at u64::MAX");
    }

    #[test]
    fn impact_sites_validate_ranges_and_report_the_schedule() {
        let mut world = app_world();
        for (k, v) in [
            ("radius", json!(0.5)),
            ("radius", json!(5_001)),
            ("depth", json!(0.0)),
            ("rim_height", json!(-1)),
            ("blanket_radius", json!(20_001)),
            ("scorch_radius", json!(0)),
            ("puff_lead", json!(-2)),
            ("at_frame", json!("later")),
        ] {
            let err = impact_site(&mut world, &json!({ "center": [0, 0, 0], k: v.clone() })).expect_err(k);
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{k}={v}");
        }
        assert!(impact_site(&mut world, &json!({})).is_err(), "center required");
        let out = impact_site(
            &mut world,
            &json!({ "center": [10, 0, 10], "at_frame": 100, "puff_lead": 500, "detail": "minimal" }),
        )
        .expect("site");
        assert_eq!(out["at_frame"], json!(100));
        assert_eq!(
            out["puff_frame"],
            json!(0),
            "a lead past frame 0 saturates, no wrap"
        );
        assert_eq!(out["detail"], json!("minimal"));
        let id = out["site"].as_u64().expect("id");
        let state = debris_state(&mut world, &json!({})).expect("state");
        assert_eq!(state["sites"][0]["site"], json!(id));
        assert_eq!(state["sites"][0]["placed"], json!(false));
        assert!(impact_site(&mut world, &json!({ "remove": id })).is_ok());
        assert!(
            impact_site(&mut world, &json!({ "remove": id })).is_err(),
            "removing twice"
        );
    }
}
