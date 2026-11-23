// SPDX-License-Identifier: MPL-2.0
//! Smart objects v2 (`docs/specs/39-smart-objects-v2.md` § 11): priorities and handles, the claim
//! life cycle and its automatic releases, search stages and the rejection log, activity and user
//! rules, conditions, entrances, runtime tags and the ordered message log. Fixed worlds, no sleeps.

use regia_ai::smart_object_def::{MAX_DEF_ENTRANCES, MAX_DEF_SLOTS};
use regia_ai::smart_object_v2::{
    ActivityMerge, Availability, ClaimHandle, ClaimPriority, ConditionCtx, ConditionRef, EntranceKind,
    HitStatus, LostReason, QueryRegion, RejectStage, SlotBehaviors, SlotChange, SlotChanged, SlotCondition,
    SlotEntrance, SlotLost, SlotQuery, SlotState, SmartObjectConditions, SmartObjectEvent, SmartObjectEvents,
    SmartObjectRegistry, SmartObjectRules, UnavailableWhy, UserRule, add_runtime_tag, begin_use,
    behavior_cache_len, claim_with, entrance_for, notify_slot, release_handle, release_handle_as,
    remove_runtime_tag, set_enabled, slot_behavior, slot_state,
};
use regia_ai::{
    RejectionLog, SmartObject, SmartObjectDef, SmartObjectDefError, SmartObjectDefinition, SmartObjectError,
    SmartObjectIndex, SmartObjectPlugin, SmartObjectSlot, claim_slot, release_slot,
};
use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3};
use regia_framework::tags::{Tag, TagContainer, TagQuery};
use regia_nav::query::NavQuery;
use regia_scene::{GlobalTransform, Transform};
use std::sync::Arc;

fn tag(name: &str) -> Tag {
    Tag::new(name).expect("a valid tag name")
}

fn tags(names: &[&str]) -> TagContainer {
    names.iter().map(|n| tag(n)).collect()
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SmartObjectPlugin));
    app
}

fn spawn_object(app: &mut App) -> Entity {
    app.world_mut().spawn(SmartObject::default()).id()
}

fn spawn_slot(app: &mut App, object: Entity, role: &str, pos: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            SmartObjectSlot::new(role),
            GlobalTransform(Affine3A::from_translation(pos)),
            ChildOf(object),
        ))
        .id()
}

/// One object with one slot at the origin, plus two agents.
fn one_slot() -> (App, Entity, Entity, Entity, Entity) {
    let mut app = app();
    let object = spawn_object(&mut app);
    let slot = spawn_slot(&mut app, object, "sit", Vec3::ZERO);
    let a = app.world_mut().spawn_empty().id();
    let b = app.world_mut().spawn_empty().id();
    (app, object, slot, a, b)
}

fn everywhere() -> QueryRegion {
    QueryRegion::Sphere {
        center: Vec3::ZERO,
        radius: 1000.0,
    }
}

fn search(app: &App, q: &SlotQuery) -> Vec<regia_ai::SlotHit> {
    app.world()
        .resource::<SmartObjectIndex>()
        .search(app.world(), q, None)
}

fn events(app: &App) -> Vec<SmartObjectEvent> {
    app.world().resource::<SmartObjectEvents>().recent(256)
}

fn lost(app: &App) -> Vec<SlotLost> {
    events(app)
        .into_iter()
        .filter_map(|e| match e {
            SmartObjectEvent::Lost(l) => Some(l),
            SmartObjectEvent::Changed(_) => None,
        })
        .collect()
}

// ---- priorities and handles ---------------------------------------------------------------

#[test]
fn priorities_preempt_only_strictly_higher_claims() {
    let (mut app, _object, slot, a, b) = one_slot();
    let c = app.world_mut().spawn_empty().id();
    let first = claim_with(app.world_mut(), slot, a, ClaimPriority::Normal).expect("free slot");
    // Equal and lower priority lose.
    assert_eq!(
        claim_with(app.world_mut(), slot, b, ClaimPriority::Normal),
        Err(SmartObjectError::SlotClaimed(slot, a))
    );
    assert_eq!(
        claim_with(app.world_mut(), slot, b, ClaimPriority::Low),
        Err(SmartObjectError::SlotClaimed(slot, a))
    );
    // A higher priority takes the slot; the old handle is stale.
    let second = claim_with(app.world_mut(), slot, c, ClaimPriority::High).expect("preempts");
    assert_ne!(first.generation, second.generation);
    assert_eq!(
        release_handle(app.world_mut(), first),
        Err(SmartObjectError::StaleHandle)
    );
    assert_eq!(
        begin_use(app.world_mut(), first),
        Err(SmartObjectError::StaleHandle)
    );
    app.update();
    assert_eq!(
        lost(&app),
        vec![SlotLost {
            handle: first,
            holder: a,
            reason: LostReason::Preempted
        }]
    );
    // The new holder is intact.
    assert_eq!(
        app.world().get::<SmartObjectSlot>(slot).expect("slot").claimed_by,
        Some(c)
    );
    release_handle(app.world_mut(), second).expect("holder releases");
}

#[test]
fn an_in_use_slot_cannot_be_preempted_and_a_same_user_reclaim_is_idempotent() {
    let (mut app, _object, slot, a, b) = one_slot();
    let handle = claim_with(app.world_mut(), slot, a, ClaimPriority::Low).expect("claims");
    // Same user: same handle, and the held priority may be raised.
    let again = claim_with(app.world_mut(), slot, a, ClaimPriority::Urgent).expect("same user");
    assert_eq!(again, handle);
    assert_eq!(
        app.world()
            .get::<SmartObjectSlot>(slot)
            .expect("slot")
            .priority_held,
        ClaimPriority::Urgent
    );
    begin_use(app.world_mut(), handle).expect("begins use");
    begin_use(app.world_mut(), handle).expect("idempotent");
    assert_eq!(slot_state(app.world(), slot), Some(SlotState::InUse));
    assert_eq!(
        claim_with(app.world_mut(), slot, b, ClaimPriority::Urgent),
        Err(SmartObjectError::SlotInUse(slot))
    );
    // The holder may still ask again.
    assert_eq!(
        claim_with(app.world_mut(), slot, a, ClaimPriority::Normal),
        Ok(handle)
    );
}

#[test]
fn availability_and_preemptable_hits_follow_the_requesters_priority() {
    let (mut app, _object, slot, a, _b) = one_slot();
    app.update();
    let index_availability = |app: &App, p| {
        app.world()
            .resource::<SmartObjectIndex>()
            .availability(app.world(), slot, p)
    };
    assert_eq!(
        index_availability(&app, ClaimPriority::Background),
        Availability::Free
    );
    let handle = claim_with(app.world_mut(), slot, a, ClaimPriority::Normal).expect("claims");
    app.update();
    assert_eq!(
        index_availability(&app, ClaimPriority::Normal),
        Availability::Unavailable(UnavailableWhy::HeldHigher)
    );
    assert_eq!(
        index_availability(&app, ClaimPriority::High),
        Availability::Preemptable
    );

    let mut q = SlotQuery::new(everywhere());
    q.allow_preempt = true;
    q.priority = ClaimPriority::Normal;
    assert!(search(&app, &q).is_empty(), "equal priority is not preemptable");
    q.priority = ClaimPriority::High;
    let hits = search(&app, &q);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].status, HitStatus::Preemptable { holder: a });
    q.allow_preempt = false;
    assert!(search(&app, &q).is_empty(), "claimed slots need allow_preempt");

    begin_use(app.world_mut(), handle).expect("in use");
    app.update();
    q.allow_preempt = true;
    q.priority = ClaimPriority::Urgent;
    assert!(search(&app, &q).is_empty(), "in-use slots are never hits");
    assert_eq!(
        index_availability(&app, ClaimPriority::Urgent),
        Availability::Unavailable(UnavailableWhy::InUse)
    );
}

#[test]
fn handles_pack_into_a_u64_and_back() {
    let (mut app, _object, slot, a, _b) = one_slot();
    let handle = claim_with(app.world_mut(), slot, a, ClaimPriority::Normal).expect("claims");
    let bits = handle.to_bits();
    assert_eq!(ClaimHandle::from_bits(app.world_mut(), bits), Some(handle));
    assert_eq!(ClaimHandle::from_bits(app.world_mut(), u64::MAX), None);
}

#[test]
fn packed_handles_to_one_index_at_two_entity_generations_differ() {
    let (mut app, _object, slot, a, _b) = one_slot();
    let old = claim_with(app.world_mut(), slot, a, ClaimPriority::Normal).expect("claims");
    // The same index one entity generation later, as after a despawn and a reuse; both are
    // first claims (generation 0).
    let later = Entity::from_bits(slot.to_bits() + (1 << 32));
    let new = ClaimHandle {
        slot: later,
        generation: old.generation,
    };
    assert_ne!(old.slot, new.slot);
    assert_ne!(old.to_bits(), new.to_bits());
    // The live slot decodes with its own entity generation mixed back out.
    assert_eq!(ClaimHandle::from_bits(app.world_mut(), old.to_bits()), Some(old));
    let from_stale = ClaimHandle::from_bits(app.world_mut(), new.to_bits()).expect("same index");
    assert_ne!(
        from_stale, old,
        "a handle from another entity generation must not resolve to this claim"
    );
    assert!(matches!(
        release_handle(app.world_mut(), from_stale),
        Err(SmartObjectError::StaleHandle)
    ));
}

#[test]
fn the_spec_31_api_still_behaves() {
    let (mut app, _object, slot, a, b) = one_slot();
    claim_slot(app.world_mut(), slot, a).expect("claims");
    claim_slot(app.world_mut(), slot, a).expect("idempotent");
    assert_eq!(
        claim_slot(app.world_mut(), slot, b),
        Err(SmartObjectError::SlotClaimed(slot, a))
    );
    assert_eq!(
        release_slot(app.world_mut(), slot, b),
        Err(SmartObjectError::NotClaimedBy(slot, a))
    );
    release_slot(app.world_mut(), slot, a).expect("releases");
    release_slot(app.world_mut(), slot, a).expect("no-op");
    claim_slot(app.world_mut(), slot, b).expect("free again");
}

#[test]
fn release_by_handle_checks_the_holder() {
    let (mut app, _object, slot, a, b) = one_slot();
    let handle = claim_with(app.world_mut(), slot, a, ClaimPriority::Normal).expect("claims");
    assert_eq!(
        release_handle_as(app.world_mut(), handle, b),
        Err(SmartObjectError::NotHolder)
    );
    release_handle_as(app.world_mut(), handle, a).expect("holder");
    assert_eq!(slot_state(app.world(), slot), Some(SlotState::Free));
}

// ---- life cycle ---------------------------------------------------------------------------

#[test]
fn the_life_cycle_runs_free_claimed_in_use_free() {
    let (mut app, _object, slot, a, _b) = one_slot();
    assert_eq!(slot_state(app.world(), slot), Some(SlotState::Free));
    let handle = claim_with(app.world_mut(), slot, a, ClaimPriority::Normal).expect("claims");
    assert_eq!(slot_state(app.world(), slot), Some(SlotState::Claimed));
    begin_use(app.world_mut(), handle).expect("begins");
    assert_eq!(slot_state(app.world(), slot), Some(SlotState::InUse));
    release_handle(app.world_mut(), handle).expect("releases");
    assert_eq!(slot_state(app.world(), slot), Some(SlotState::Free));
    assert_eq!(slot_state(app.world(), Entity::PLACEHOLDER), None);
}

#[test]
fn every_automatic_release_writes_exactly_one_slot_lost() {
    let mut app = app();
    let object = spawn_object(&mut app);
    let holder_gone = spawn_slot(&mut app, object, "a", Vec3::ZERO);
    let slot_gone = spawn_slot(&mut app, object, "b", Vec3::X);
    let disabled = spawn_slot(&mut app, object, "c", Vec3::Y);
    let h1 = app.world_mut().spawn_empty().id();
    let h2 = app.world_mut().spawn_empty().id();
    let h3 = app.world_mut().spawn_empty().id();
    let c1 = claim_with(app.world_mut(), holder_gone, h1, ClaimPriority::Normal).expect("claims");
    let c2 = claim_with(app.world_mut(), slot_gone, h2, ClaimPriority::Normal).expect("claims");
    let c3 = claim_with(app.world_mut(), disabled, h3, ClaimPriority::Normal).expect("claims");
    app.world_mut().despawn(h1);
    app.world_mut().despawn(slot_gone);
    set_enabled(app.world_mut(), disabled, false, "test").expect("a slot");
    app.update();

    let mut seen = lost(&app);
    seen.sort_by_key(|l| l.handle.slot.to_bits());
    let mut expected = vec![
        SlotLost {
            handle: c1,
            holder: h1,
            reason: LostReason::HolderGone,
        },
        SlotLost {
            handle: c2,
            holder: h2,
            reason: LostReason::SlotGone,
        },
        SlotLost {
            handle: c3,
            holder: h3,
            reason: LostReason::Disabled,
        },
    ];
    expected.sort_by_key(|l| l.handle.slot.to_bits());
    assert_eq!(seen, expected);
    // A second frame does not repeat them, and the surviving slots are free with a new generation.
    app.update();
    assert_eq!(lost(&app).len(), 3);
    assert_eq!(slot_state(app.world(), holder_gone), Some(SlotState::Free));
    assert_ne!(
        app.world()
            .get::<SmartObjectSlot>(holder_gone)
            .expect("slot")
            .generation,
        c1.generation
    );
    assert_eq!(
        release_handle(app.world_mut(), c1),
        Err(SmartObjectError::StaleHandle)
    );
}

#[test]
fn two_off_reasons_need_two_enables() {
    let (mut app, object, slot, _a, _b) = one_slot();
    assert_eq!(set_enabled(app.world_mut(), slot, false, "a"), Some(false));
    assert_eq!(set_enabled(app.world_mut(), slot, false, "b"), Some(false));
    assert_eq!(set_enabled(app.world_mut(), slot, true, "a"), Some(false));
    assert_eq!(slot_state(app.world(), slot), Some(SlotState::Disabled));
    assert_eq!(set_enabled(app.world_mut(), slot, true, "b"), Some(true));
    assert_eq!(slot_state(app.world(), slot), Some(SlotState::Free));
    // An object's off-reason disables its slots too.
    set_enabled(app.world_mut(), object, false, "o").expect("an object");
    assert_eq!(slot_state(app.world(), slot), Some(SlotState::Disabled));
    assert_eq!(
        set_enabled(app.world_mut(), Entity::PLACEHOLDER, false, "x"),
        None
    );
}

// ---- search -------------------------------------------------------------------------------

#[test]
fn results_sort_by_distance_then_entity_and_the_limit_applies_after() {
    let mut app = app();
    let object = spawn_object(&mut app);
    let far = spawn_slot(&mut app, object, "sit", Vec3::new(3.0, 0.0, 0.0));
    let near = spawn_slot(&mut app, object, "sit", Vec3::new(1.0, 0.0, 0.0));
    let mid = spawn_slot(&mut app, object, "sit", Vec3::new(2.0, 0.0, 0.0));
    let outside = spawn_slot(&mut app, object, "sit", Vec3::new(30.0, 0.0, 0.0));
    app.update();
    let mut q = SlotQuery::new(QueryRegion::Sphere {
        center: Vec3::ZERO,
        radius: 5.0,
    });
    let order: Vec<Entity> = search(&app, &q).iter().map(|h| h.slot).collect();
    assert_eq!(order, vec![near, mid, far]);
    q.limit = Some(2);
    let order: Vec<Entity> = search(&app, &q).iter().map(|h| h.slot).collect();
    assert_eq!(order, vec![near, mid]);
    let first = app
        .world()
        .resource::<SmartObjectIndex>()
        .find_first(app.world(), &q)
        .expect("a hit");
    assert_eq!(first.slot, near);
    assert_eq!(first.object, Some(object));
    assert!((first.distance - 1.0).abs() < 1e-6);
    // A box region reaches the far slot only when it covers it.
    q.limit = None;
    q.region = QueryRegion::Aabb {
        min: Vec3::splat(-1.0),
        max: Vec3::new(40.0, 1.0, 1.0),
    };
    assert!(search(&app, &q).iter().any(|h| h.slot == outside));
}

#[test]
fn the_first_failing_stage_is_recorded_and_the_log_does_not_change_the_results() {
    let mut app = app();
    let object = spawn_object(&mut app);
    let ok = spawn_slot(&mut app, object, "sit", Vec3::X);
    // Fails the region and the family stage; only the region is recorded.
    let outside = spawn_slot(&mut app, object, "sit", Vec3::new(500.0, 0.0, 0.0));
    let claimed = spawn_slot(&mut app, object, "sit", Vec3::Y);
    let holder = app.world_mut().spawn_empty().id();
    claim_with(app.world_mut(), claimed, holder, ClaimPriority::Normal).expect("claims");
    for slot in [ok, outside, claimed] {
        app.world_mut().entity_mut(slot).insert(SlotBehaviors(
            [("sit".to_string(), "()".to_string())].into_iter().collect(),
        ));
    }
    app.world_mut()
        .entity_mut(outside)
        .insert(SlotBehaviors::default());
    app.update();

    let mut q = SlotQuery::new(QueryRegion::Sphere {
        center: Vec3::ZERO,
        radius: 10.0,
    });
    q.families = vec!["lean".to_string()];
    // Nobody offers "lean": the claimed slot fails at `state` before `family`, the open one at `family`.
    let mut log = RejectionLog::default();
    let with_log = app
        .world()
        .resource::<SmartObjectIndex>()
        .search(app.world(), &q, Some(&mut log));
    assert!(with_log.is_empty());
    assert_eq!(with_log, search(&app, &q));
    let stage_of = |slot: Entity| log.entries.iter().find(|r| r.slot == Some(slot)).map(|r| r.stage);
    assert_eq!(stage_of(outside), Some(RejectStage::Region));
    assert_eq!(stage_of(claimed), Some(RejectStage::State));
    assert_eq!(stage_of(ok), Some(RejectStage::Family));

    q.families = vec!["sit".to_string()];
    let hits: Vec<Entity> = search(&app, &q).iter().map(|h| h.slot).collect();
    assert_eq!(hits, vec![ok]);
}

#[test]
fn a_non_finite_sibling_does_not_stop_the_object_level_collapse() {
    let mut app = app();
    let off = spawn_object(&mut app);
    spawn_slot(&mut app, off, "sit", Vec3::Y);
    spawn_slot(&mut app, off, "sit", Vec3::Z);
    spawn_slot(&mut app, off, "sit", Vec3::new(f32::NAN, 0.0, 0.0));
    set_enabled(app.world_mut(), off, false, "closed").expect("object");
    app.update();
    let mut log = RejectionLog::default();
    app.world().resource::<SmartObjectIndex>().search(
        app.world(),
        &SlotQuery::new(everywhere()),
        Some(&mut log),
    );
    assert_eq!(log.entries.len(), 1, "{:?}", log.entries);
    assert_eq!(log.entries[0].slot, None);
}

#[test]
fn a_huge_aabb_reference_does_not_overflow() {
    let region = QueryRegion::Aabb {
        min: Vec3::splat(2.0e38),
        max: Vec3::splat(3.0e38),
    };
    let r = region.reference();
    assert!(r.is_finite());
    assert!((r.x / 2.5e38 - 1.0).abs() < 1e-6);
    let mut app = app();
    let object = spawn_object(&mut app);
    let near = spawn_slot(&mut app, object, "sit", Vec3::new(2.5e38, 2.5e38, 2.5e38));
    spawn_slot(&mut app, object, "sit", Vec3::new(2.9e38, 2.9e38, 2.9e38));
    app.update();
    let hits = search(&app, &SlotQuery::new(region));
    assert!(hits.iter().all(|h| h.distance.is_finite()), "{hits:?}");
    assert_eq!(hits[0].slot, near);
}

#[test]
fn an_object_level_failure_logs_one_entry_without_a_slot() {
    let mut app = app();
    let open = spawn_object(&mut app);
    let off = spawn_object(&mut app);
    spawn_slot(&mut app, open, "sit", Vec3::X);
    spawn_slot(&mut app, off, "sit", Vec3::Y);
    spawn_slot(&mut app, off, "sit", Vec3::Z);
    set_enabled(app.world_mut(), off, false, "closed").expect("object");
    app.update();
    let q = SlotQuery::new(everywhere());
    let mut log = RejectionLog::default();
    let hits = app
        .world()
        .resource::<SmartObjectIndex>()
        .search(app.world(), &q, Some(&mut log));
    assert_eq!(hits.len(), 1);
    assert_eq!(
        log.entries.len(),
        1,
        "two slots, one object-level entry: {:?}",
        log.entries
    );
    assert_eq!(log.entries[0].slot, None);
    assert_eq!(log.entries[0].object, Some(off));
    assert_eq!(log.entries[0].stage, RejectStage::Enabled);

    let mut q = SlotQuery::new(everywhere());
    q.include_disabled = true;
    assert_eq!(search(&app, &q).len(), 3);
}

#[test]
fn activity_sets_merge_per_the_rule_and_runtime_tags_join() {
    let mut app = app();
    let object = spawn_object(&mut app);
    let slot = spawn_slot(&mut app, object, "sit", Vec3::X);
    app.world_mut().entity_mut(object).insert(SmartObjectRules {
        activity: tags(&["Activity.Sit"]),
        ..SmartObjectRules::default()
    });
    app.world_mut()
        .get_mut::<SmartObjectSlot>(slot)
        .expect("slot")
        .activity = tags(&["Activity.Rest"]);
    app.update();

    let wants = |names: &str| {
        let mut q = SlotQuery::new(everywhere());
        q.activity = Some(TagQuery::Has(tag(names)));
        q
    };
    // Union: both sets.
    assert_eq!(search(&app, &wants("Activity.Sit")).len(), 1);
    assert_eq!(search(&app, &wants("Activity.Rest")).len(), 1);
    // SlotFirst: the slot's own tags win while it has any.
    app.world_mut()
        .get_mut::<SmartObjectRules>(object)
        .expect("rules")
        .activity_merge = ActivityMerge::SlotFirst;
    assert!(search(&app, &wants("Activity.Sit")).is_empty());
    assert_eq!(search(&app, &wants("Activity.Rest")).len(), 1);
    // Runtime tags always join, whichever merge rule is on.
    assert!(add_runtime_tag(app.world_mut(), slot, tag("Activity.Talk")));
    assert!(
        !add_runtime_tag(app.world_mut(), slot, tag("Activity.Talk")),
        "no change, no message"
    );
    assert_eq!(search(&app, &wants("Activity.Talk")).len(), 1);
    assert!(remove_runtime_tag(app.world_mut(), slot, tag("Activity.Talk")));
    assert!(search(&app, &wants("Activity.Talk")).is_empty());
    // An empty slot set falls back to the definition's.
    app.world_mut()
        .get_mut::<SmartObjectSlot>(slot)
        .expect("slot")
        .activity = TagContainer::new();
    assert_eq!(search(&app, &wants("Activity.Sit")).len(), 1);
}

#[test]
fn user_rules_combine_the_definition_and_slot_queries() {
    let mut app = app();
    let object = spawn_object(&mut app);
    let slot = spawn_slot(&mut app, object, "sit", Vec3::X);
    let guard = TagQuery::Has(tag("Role.Guard"));
    let civilian = TagQuery::Has(tag("Role.Civilian"));
    app.world_mut().entity_mut(object).insert(SmartObjectRules {
        user_query: Some(guard),
        ..SmartObjectRules::default()
    });
    app.world_mut()
        .get_mut::<SmartObjectSlot>(slot)
        .expect("slot")
        .user_query = Some(civilian);
    app.update();
    let with_tags = |names: &[&str]| {
        let mut q = SlotQuery::new(everywhere());
        q.user_tags = tags(names);
        q
    };
    let set_rule = |app: &mut App, rule| {
        app.world_mut()
            .get_mut::<SmartObjectRules>(object)
            .expect("rules")
            .user_rule = rule;
    };
    // DefinitionAndSlot: both must pass.
    assert!(search(&app, &with_tags(&["Role.Guard"])).is_empty());
    assert!(search(&app, &with_tags(&["Role.Civilian"])).is_empty());
    assert_eq!(
        search(&app, &with_tags(&["Role.Guard", "Role.Civilian"])).len(),
        1
    );
    // SlotOnly: the definition's query is ignored.
    set_rule(&mut app, UserRule::SlotOnly);
    assert_eq!(search(&app, &with_tags(&["Role.Civilian"])).len(), 1);
    assert!(search(&app, &with_tags(&["Role.Guard"])).is_empty());
    // SlotElseDefinition: the slot's query wins while it exists, then the definition's applies.
    set_rule(&mut app, UserRule::SlotElseDefinition);
    assert_eq!(search(&app, &with_tags(&["Role.Civilian"])).len(), 1);
    app.world_mut()
        .get_mut::<SmartObjectSlot>(slot)
        .expect("slot")
        .user_query = None;
    assert_eq!(search(&app, &with_tags(&["Role.Guard"])).len(), 1);
    assert!(search(&app, &with_tags(&["Role.Civilian"])).is_empty());
    // No queries at all pass everyone.
    app.world_mut()
        .get_mut::<SmartObjectRules>(object)
        .expect("rules")
        .user_query = None;
    assert_eq!(search(&app, &with_tags(&[])).len(), 1);
}

struct Fixed(bool);

impl SlotCondition for Fixed {
    fn test(&self, _ctx: &ConditionCtx<'_>) -> bool {
        self.0
    }
}

fn cond(path: &str) -> SmartObjectConditions {
    SmartObjectConditions(vec![ConditionRef {
        path: path.to_string(),
        ..ConditionRef::default()
    }])
}

#[test]
fn conditions_run_only_on_request_and_an_unregistered_path_rejects() {
    let mut app = app();
    let object = spawn_object(&mut app);
    let pass = spawn_slot(&mut app, object, "sit", Vec3::X);
    let fail = spawn_slot(&mut app, object, "sit", Vec3::Y);
    let missing = spawn_slot(&mut app, object, "sit", Vec3::Z);
    {
        let mut registry = app.world_mut().resource_mut::<SmartObjectRegistry>();
        registry.register_condition("t::Yes", Fixed(true));
        registry.register_condition("t::No", Fixed(false));
    }
    app.world_mut().entity_mut(pass).insert(cond("t::Yes"));
    app.world_mut().entity_mut(fail).insert(cond("t::No"));
    app.world_mut().entity_mut(missing).insert(cond("t::Nope"));
    app.update();

    let mut q = SlotQuery::new(everywhere());
    let hits: Vec<Entity> = search(&app, &q).iter().map(|h| h.slot).collect();
    assert_eq!(hits, vec![pass]);
    let mut log = RejectionLog::default();
    let _ = app
        .world()
        .resource::<SmartObjectIndex>()
        .search(app.world(), &q, Some(&mut log));
    assert_eq!(log.entries.len(), 2);
    assert!(log.entries.iter().all(|r| r.stage == RejectStage::Condition));
    // Switched off, conditions are not evaluated at all.
    q.check_conditions = false;
    assert_eq!(search(&app, &q).len(), 3);
    // An object-level condition applies to every slot (and is logged once).
    app.world_mut().entity_mut(object).insert(cond("t::No"));
    q.check_conditions = true;
    assert!(search(&app, &q).is_empty());
    let mut log = RejectionLog::default();
    let _ = app
        .world()
        .resource::<SmartObjectIndex>()
        .search(app.world(), &q, Some(&mut log));
    assert_eq!(log.entries.len(), 1);
    assert_eq!(log.entries[0].slot, None);
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
struct Pose {
    pose: String,
}

#[test]
fn slot_behavior_decodes_caches_and_fails_softly() {
    let (mut app, _object, slot, _a, _b) = one_slot();
    app.world_mut()
        .resource_mut::<SmartObjectRegistry>()
        .register_behavior::<Pose>("sit");
    app.world_mut().entity_mut(slot).insert(SlotBehaviors(
        [
            ("sit".to_string(), "(pose: \"alert\")".to_string()),
            ("broken".to_string(), "(pose: ".to_string()),
            ("unknown".to_string(), "()".to_string()),
        ]
        .into_iter()
        .collect(),
    ));
    app.world_mut()
        .resource_mut::<SmartObjectRegistry>()
        .register_behavior::<Pose>("broken");
    let pose = Pose {
        pose: "alert".to_string(),
    };
    assert_eq!(
        slot_behavior::<Pose>(app.world(), slot, "sit"),
        Some(pose.clone())
    );
    assert_eq!(slot_behavior::<Pose>(app.world(), slot, "sit"), Some(pose));
    assert_eq!(slot_behavior::<Pose>(app.world(), slot, "broken"), None);
    assert_eq!(slot_behavior::<Pose>(app.world(), slot, "unknown"), None);
    assert_eq!(slot_behavior::<Pose>(app.world(), slot, "absent"), None);
    // The wrong type for a registered family is also a soft failure.
    assert_eq!(slot_behavior::<u8>(app.world(), slot, "sit"), None);
    // Editing the payload invalidates the cache.
    app.world_mut()
        .get_mut::<SlotBehaviors>(slot)
        .expect("behaviors")
        .0
        .insert("sit".to_string(), "(pose: \"relaxed\")".to_string());
    assert_eq!(
        slot_behavior::<Pose>(app.world(), slot, "sit").map(|p| p.pose),
        Some("relaxed".to_string())
    );
}

/// Audit-gameplay zai L3 (ADR-1809): despawned slots and removed families leave the cache.
#[test]
fn the_behavior_cache_forgets_despawned_slots_and_removed_families() {
    let (mut app, object, slot, _a, _b) = one_slot();
    app.world_mut()
        .resource_mut::<SmartObjectRegistry>()
        .register_behavior::<Pose>("sit");
    app.world_mut()
        .resource_mut::<SmartObjectRegistry>()
        .register_behavior::<Pose>("lie");
    let behaviors = |pose: &str| {
        SlotBehaviors(
            [
                ("sit".to_string(), format!("(pose: \"{pose}\")")),
                ("lie".to_string(), "(pose: \"flat\")".to_string()),
            ]
            .into_iter()
            .collect(),
        )
    };
    app.world_mut().entity_mut(slot).insert(behaviors("alert"));
    let other = spawn_slot(&mut app, object, "sit", Vec3::X);
    app.world_mut().entity_mut(other).insert(behaviors("calm"));
    for s in [slot, other] {
        assert!(slot_behavior::<Pose>(app.world(), s, "sit").is_some());
        assert!(slot_behavior::<Pose>(app.world(), s, "lie").is_some());
    }
    assert_eq!(behavior_cache_len(app.world()), 4);
    app.update();
    assert_eq!(behavior_cache_len(app.world()), 4, "live entries stay");

    app.world_mut()
        .get_mut::<SlotBehaviors>(slot)
        .expect("behaviors")
        .0
        .remove("lie");
    app.world_mut().despawn(other);
    app.update();
    assert_eq!(behavior_cache_len(app.world()), 1, "only `slot`/`sit` is left");
    assert_eq!(
        slot_behavior::<Pose>(app.world(), slot, "sit").map(|p| p.pose),
        Some("alert".to_string())
    );
}

// ---- entrances ----------------------------------------------------------------------------

fn spawn_entrance(app: &mut App, slot: Entity, kind: EntranceKind, rank: i8, pos: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            SlotEntrance { kind, rank },
            GlobalTransform(Affine3A::from_translation(pos)),
            ChildOf(slot),
        ))
        .id()
}

#[test]
fn entrances_pick_the_highest_rank_then_distance() {
    let (mut app, _object, slot, _a, _b) = one_slot();
    spawn_entrance(&mut app, slot, EntranceKind::Enter, 0, Vec3::new(1.0, 0.0, 0.0));
    spawn_entrance(&mut app, slot, EntranceKind::Enter, 1, Vec3::new(5.0, 0.0, 0.0));
    spawn_entrance(&mut app, slot, EntranceKind::Enter, 1, Vec3::new(3.0, 0.0, 0.0));
    spawn_entrance(&mut app, slot, EntranceKind::Exit, 9, Vec3::new(-7.0, 0.0, 0.0));
    spawn_entrance(&mut app, slot, EntranceKind::Both, 1, Vec3::new(4.0, 0.0, 0.0));
    app.update();
    let from = Vec3::ZERO;
    // Enter: rank 1 wins; among rank 1 (5, 3 and the Both one at 4) the nearest to `from` is 3.
    assert_eq!(
        entrance_for(app.world(), slot, from, EntranceKind::Enter, None),
        Some(Vec3::new(3.0, 0.0, 0.0))
    );
    // Exit: the rank-9 exit beats the Both entrance.
    assert_eq!(
        entrance_for(app.world(), slot, from, EntranceKind::Exit, None),
        Some(Vec3::new(-7.0, 0.0, 0.0))
    );
    // Without entrances there is nothing to return.
    let (app2, _o, slot2, _a2, _b2) = one_slot();
    assert_eq!(
        entrance_for(app2.world(), slot2, from, EntranceKind::Enter, None),
        None
    );
}

/// Spec 39 test 8: with a navigation query the point lands on the mesh, and an entrance that
/// cannot be projected is skipped however high its rank.
#[test]
fn entrances_project_onto_the_mesh_and_skip_what_they_cannot_reach() {
    let (mut app, _object, slot, _a, _b) = one_slot();
    // Unreachable: far outside the floor, whatever its rank.
    spawn_entrance(&mut app, slot, EntranceKind::Enter, 9, Vec3::new(40.0, 0.0, 40.0));
    // On the floor's footprint, floating above it: lands on the floor.
    spawn_entrance(&mut app, slot, EntranceKind::Enter, 1, Vec3::new(2.0, 0.5, 2.0));
    app.update();
    let mesh = tiny_navmesh();
    let query = NavQuery {
        mesh: &mesh,
        obstacles: None,
    };
    let extents = Vec3::new(1.0, 2.0, 1.0);
    let from = Vec3::ZERO;
    let point = entrance_for(
        app.world(),
        slot,
        from,
        EntranceKind::Enter,
        Some((&query, extents)),
    )
    .expect("the reachable entrance");
    assert!(
        (point.x - 2.0).abs() < 0.5 && point.y.abs() < 0.5 && (point.z - 2.0).abs() < 0.5,
        "on the floor under the entrance: {point:?}"
    );
    // Without a query the rank decides and the raw point comes back.
    assert_eq!(
        entrance_for(app.world(), slot, from, EntranceKind::Enter, None),
        Some(Vec3::new(40.0, 0.0, 40.0))
    );
    // Nothing reachable at all: no entrance, not a raw point.
    let unreachable = Vec3::new(0.01, 0.01, 0.01);
    assert_eq!(
        entrance_for(
            app.world(),
            slot,
            Vec3::ZERO,
            EntranceKind::Exit,
            Some((&query, unreachable))
        ),
        None
    );
}

/// A square of floor at height 0, 8 m on a side, from the public builder.
fn tiny_navmesh() -> regia_nav::mesh::NavMesh {
    let corner = |x: f32, z: f32| Vec3::new(x, 0.0, z);
    let floor = [
        [corner(-4.0, -4.0), corner(4.0, 4.0), corner(4.0, -4.0)],
        [corner(-4.0, -4.0), corner(-4.0, 4.0), corner(4.0, 4.0)],
    ];
    regia_nav::NavMeshBuilder::default().build(&floor).expect("floor")
}

// ---- messages -----------------------------------------------------------------------------

#[test]
fn a_scripted_scenario_yields_an_exact_ordered_message_vector() {
    let (mut app, object, slot, a, _b) = one_slot();
    let changed = |what, t| {
        SmartObjectEvent::Changed(SlotChanged {
            object: Some(object),
            slot: Some(slot),
            what,
            tag: t,
        })
    };
    let handle = claim_with(app.world_mut(), slot, a, ClaimPriority::Normal).expect("claims");
    begin_use(app.world_mut(), handle).expect("uses");
    release_handle(app.world_mut(), handle).expect("releases");
    let t = tag("Activity.Sit");
    assert!(add_runtime_tag(app.world_mut(), slot, t));
    assert!(remove_runtime_tag(app.world_mut(), slot, t));
    notify_slot(app.world_mut(), slot, t);
    set_enabled(app.world_mut(), slot, false, "x");
    set_enabled(app.world_mut(), slot, false, "y"); // already off: no message
    set_enabled(app.world_mut(), slot, true, "x"); // still off: no message
    set_enabled(app.world_mut(), slot, true, "y");
    assert_eq!(app.world().resource::<SmartObjectEvents>().pending_len(), 8);
    app.update();
    assert_eq!(app.world().resource::<SmartObjectEvents>().pending_len(), 0);
    assert_eq!(
        events(&app),
        vec![
            changed(SlotChange::Claimed, None),
            changed(SlotChange::InUse, None),
            changed(SlotChange::Released, None),
            changed(SlotChange::TagAdded, Some(t)),
            changed(SlotChange::TagRemoved, Some(t)),
            changed(SlotChange::Custom, Some(t)),
            changed(SlotChange::Disabled, None),
            changed(SlotChange::Enabled, None),
        ]
    );
}

#[test]
fn messages_are_ordered_by_object_then_slot() {
    let mut app = app();
    let o1 = spawn_object(&mut app);
    let o2 = spawn_object(&mut app);
    let s1 = spawn_slot(&mut app, o1, "sit", Vec3::ZERO);
    let s2 = spawn_slot(&mut app, o2, "sit", Vec3::X);
    let user = app.world_mut().spawn_empty().id();
    // Claim the later object's slot first; the log still lists the earlier object first.
    claim_with(app.world_mut(), s2, user, ClaimPriority::Normal).expect("claims");
    claim_with(app.world_mut(), s1, user, ClaimPriority::Normal).expect("claims");
    app.update();
    let slots: Vec<Option<Entity>> = events(&app)
        .into_iter()
        .filter_map(|e| match e {
            SmartObjectEvent::Changed(c) => Some(c.slot),
            SmartObjectEvent::Lost(_) => None,
        })
        .collect();
    let mut expected = vec![(o1, s1), (o2, s2)];
    expected.sort_by_key(|(o, s)| (o.to_bits(), s.to_bits()));
    assert_eq!(
        slots,
        expected.into_iter().map(|(_, s)| Some(s)).collect::<Vec<_>>()
    );
}

#[test]
fn the_event_log_is_bounded() {
    let (mut app, _object, slot, a, _b) = one_slot();
    for _ in 0..300 {
        let handle = claim_with(app.world_mut(), slot, a, ClaimPriority::Normal).expect("claims");
        release_handle(app.world_mut(), handle).expect("releases");
    }
    app.update();
    let log = app.world().resource::<SmartObjectEvents>();
    assert_eq!(log.recent(1000).len(), 256);
    assert_eq!(log.recent(3).len(), 3);
}

// ---- definition format and materialization ------------------------------------------------

const TWO_SLOTS: &str = r#"SmartObjectDef(
    format_version: 1,
    activity: ["Activity.Sit"],
    behaviors: [(family: "sit", data: "(pose: \"default\")")],
    slots: [
        (name: "left", offset: (translation: (-0.4, 0.0, 0.0)),
         behaviors: [(family: "sit", data: "(pose: \"relaxed\")")],
         entrances: [(offset: (translation: (-0.4, 0.0, 1.0)), rank: 2)]),
        (name: "right", offset: (translation: (0.4, 0.0, 0.0))),
    ],
)"#;

fn def_of(text: &str) -> Arc<SmartObjectDef> {
    Arc::new(SmartObjectDef::from_ron(text).expect("a valid definition"))
}

fn children_of(app: &App, entity: Entity) -> Vec<Entity> {
    app.world()
        .get::<Children>(entity)
        .map(|c| c.iter().collect())
        .unwrap_or_default()
}

fn slots_of(app: &App, object: Entity) -> Vec<Entity> {
    children_of(app, object)
        .into_iter()
        .filter(|c| app.world().get::<SmartObjectSlot>(*c).is_some())
        .collect()
}

fn slot_by_def(app: &App, object: Entity, index: u16) -> Option<Entity> {
    slots_of(app, object)
        .into_iter()
        .find(|s| app.world().get::<SmartObjectSlot>(*s).and_then(|x| x.def_slot) == Some(index))
}

#[test]
fn a_definition_round_trips_and_refuses_what_the_spec_forbids() {
    let def = SmartObjectDef::from_ron(TWO_SLOTS).expect("parses");
    assert_eq!(def.slots.len(), 2);
    assert_eq!(def.slots[0].entrances[0].rank, 2);
    let again = SmartObjectDef::from_ron(&def.to_ron().expect("writes")).expect("reads back");
    assert_eq!(again, def);

    let reason = |text: &str| SmartObjectDef::from_ron(text).expect_err("refused").reason();
    assert_eq!(reason("(format_version: 2, slots: [])"), "unsupported_version");
    assert_eq!(reason("(format_version: 1, slots: ["), "bad_definition");
    assert_eq!(
        reason("(format_version: 1, slots: [(name: \"a\"), (name: \"a\")])"),
        "bad_definition"
    );
    assert_eq!(
        reason("(format_version: 1, slots: [(name: \"has space\")])"),
        "bad_definition"
    );
    assert_eq!(
        reason("(format_version: 1, slots: [(name: \"\")])"),
        "bad_definition"
    );
    let long = "x".repeat(33);
    assert_eq!(
        reason(&format!("(format_version: 1, slots: [(name: \"{long}\")])")),
        "bad_definition"
    );
    assert_eq!(
        reason("(format_version: 1, slots: [(name: \"a\", offset: (rotation: (0.0, 0.0, 0.0, 0.0)))])"),
        "bad_definition"
    );
    let many: Vec<String> = (0..=MAX_DEF_SLOTS).map(|i| format!("(name: \"s{i}\")")).collect();
    assert_eq!(
        reason(&format!("(format_version: 1, slots: [{}])", many.join(","))),
        "bad_definition"
    );
    let entrances = ["(rank: 1)"; MAX_DEF_ENTRANCES + 1].join(",");
    assert_eq!(
        reason(&format!(
            "(format_version: 1, slots: [(name: \"a\", entrances: [{entrances}])])"
        )),
        "bad_definition"
    );
    // A newer version is refused whatever the body looks like.
    assert!(matches!(
        SmartObjectDef::from_ron("(format_version: 9, slots: \"future\")"),
        Err(SmartObjectDefError::UnsupportedVersion {
            found: 9,
            supported: 1
        })
    ));
}

#[test]
fn materialization_spawns_one_child_per_slot_with_the_offsets() {
    let mut app = app();
    let object = app
        .world_mut()
        .spawn(SmartObjectDefinition(def_of(TWO_SLOTS)))
        .id();
    app.update();
    let left = slot_by_def(&app, object, 0).expect("left");
    let right = slot_by_def(&app, object, 1).expect("right");
    assert_eq!(
        app.world().get::<SmartObjectSlot>(left).expect("slot").role,
        "left"
    );
    assert_eq!(
        app.world().get::<Transform>(left).expect("transform").translation,
        Vec3::new(-0.4, 0.0, 0.0)
    );
    assert_eq!(
        app.world()
            .get::<Transform>(right)
            .expect("transform")
            .translation,
        Vec3::new(0.4, 0.0, 0.0)
    );
    // The definition's activity applies, and the slot's behavior beats the definition's.
    let behaviors = |slot| {
        app.world()
            .get::<SlotBehaviors>(slot)
            .expect("behaviors")
            .0
            .clone()
    };
    assert_eq!(behaviors(left)["sit"], "(pose: \"relaxed\")");
    assert_eq!(behaviors(right)["sit"], "(pose: \"default\")");
    assert!(app.world().get::<SmartObjectRules>(object).is_some());
    // The entrance is a child of the slot.
    let entrance = children_of(&app, left)
        .into_iter()
        .find(|c| app.world().get::<SlotEntrance>(*c).is_some())
        .expect("an entrance");
    assert_eq!(
        app.world().get::<SlotEntrance>(entrance).expect("entrance").rank,
        2
    );
    // Another frame changes nothing.
    app.update();
    assert_eq!(slots_of(&app, object).len(), 2);
}

/// corr-low finding 12: definitions that arrive in the same frame materialized in entity-bits
/// order (the complement of the index), so the slots' arrivals — the search tie-break —
/// followed the record-time ids, which two runs of the same level can allocate differently.
/// Materialization follows the definitions' arrival order instead.
#[test]
fn materialization_follows_definition_arrival_not_entity_bits() {
    let mut app = app();
    // Symmetric "seat" slots one metre either side of the query point: equal distance, so the
    // search tie-break (the slot arrival) decides.
    let def = |x: f32| {
        def_of(&format!(
            r#"(format_version: 1, slots: [(name: "seat", offset: (translation: ({x}, 0.0, 0.0)))])"#
        ))
    };
    let low = app.world_mut().spawn_empty().id();
    let high = app.world_mut().spawn_empty().id();
    // The definition arrives on the lower-id entity first. A run that allocates the ids the
    // other way around (record-time ids are thread-timing order) must not flip the winner; the
    // entity-bits order alone would, because it is the complement of the index.
    app.world_mut()
        .entity_mut(low)
        .insert(SmartObjectDefinition(def(-1.0)));
    app.world_mut()
        .entity_mut(high)
        .insert(SmartObjectDefinition(def(1.0)));
    app.update();
    let q = SlotQuery::new(QueryRegion::Sphere {
        center: Vec3::ZERO,
        radius: 10.0,
    });
    let first = app
        .world()
        .resource::<SmartObjectIndex>()
        .find_first(app.world(), &q)
        .expect("a hit");
    assert!((first.distance - 1.0).abs() < 1e-6, "the tie is real: {first:?}");
    assert_eq!(
        first.object,
        Some(low),
        "the earlier definition wins the distance tie, not the higher id {high}"
    );
}

#[test]
fn a_reload_reconciles_by_definition_slot_and_keeps_survivors() {
    let mut app = app();
    let object = app
        .world_mut()
        .spawn(SmartObjectDefinition(def_of(TWO_SLOTS)))
        .id();
    app.update();
    let left = slot_by_def(&app, object, 0).expect("left");
    let right = slot_by_def(&app, object, 1).expect("right");
    let holder = app.world_mut().spawn_empty().id();
    let right_claim = claim_with(app.world_mut(), right, holder, ClaimPriority::Normal).expect("claims");
    let left_user = app.world_mut().spawn_empty().id();
    claim_with(app.world_mut(), left, left_user, ClaimPriority::High).expect("claims");

    // The new definition keeps "left" (moved), drops "right" and adds "back".
    let next = def_of(
        r#"(format_version: 1, slots: [
            (name: "left", offset: (translation: (-1.0, 0.0, 0.0))),
        ])"#,
    );
    app.world_mut()
        .entity_mut(object)
        .insert(SmartObjectDefinition(next));
    app.update();
    assert_eq!(
        slot_by_def(&app, object, 0),
        Some(left),
        "the survivor keeps its entity id"
    );
    assert_eq!(
        app.world().get::<Transform>(left).expect("transform").translation,
        Vec3::new(-1.0, 0.0, 0.0)
    );
    assert_eq!(
        app.world().get::<SmartObjectSlot>(left).expect("slot").claimed_by,
        Some(left_user),
        "and its claim"
    );
    assert!(
        app.world().get_entity(right).is_err(),
        "the removed slot despawned"
    );
    let removed: Vec<SlotLost> = lost(&app)
        .into_iter()
        .filter(|l| l.reason == LostReason::SlotRemoved)
        .collect();
    assert_eq!(
        removed,
        vec![SlotLost {
            handle: right_claim,
            holder,
            reason: LostReason::SlotRemoved
        }]
    );

    // A third definition adds slots after the first.
    let third = def_of(r#"(format_version: 1, slots: [(name: "left"), (name: "mid"), (name: "back")])"#);
    app.world_mut()
        .entity_mut(object)
        .insert(SmartObjectDefinition(third));
    app.update();
    assert_eq!(slots_of(&app, object).len(), 3);
    assert_eq!(slot_by_def(&app, object, 0), Some(left));
    assert!(slot_by_def(&app, object, 2).is_some());
}

// ---- index scale ---------------------------------------------------------------------------

/// A small deterministic generator (SplitMix64) so the scenes are seeded and portable.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / 16_777_216.0
    }

    fn point(&mut self, extent: f32) -> Vec3 {
        Vec3::new(self.unit() * extent, self.unit() * extent, self.unit() * extent)
    }
}

/// The hits a brute-force scan of `slots` gives for a region query over free, enabled slots.
/// Every slot in this file's scenes shares one role, so the index order is the spawn order of
/// this vec: a stable sort by distance already ties in the index order (role, then arrival).
fn brute_force(app: &App, slots: &[(Entity, Vec3)], region: QueryRegion) -> Vec<(Entity, f32)> {
    let reference = region.reference();
    let mut out: Vec<(Entity, f32)> = slots
        .iter()
        .filter(|(_, p)| region.contains(*p))
        .map(|(e, p)| (*e, (*p - reference).length()))
        .collect();
    let _ = app;
    // Stable sort over the spawn-ordered vec. An exact distance tie keeps the index order
    // (ADR-4651, never the entity id and never the grid's cell-visitation order): a search ties
    // on the slot's ordinal in the index, which is this order.
    out.sort_by(|a, b| a.1.total_cmp(&b.1));
    out
}

fn hits_of(app: &App, region: QueryRegion) -> Vec<(Entity, f32)> {
    search(app, &SlotQuery::new(region))
        .iter()
        .map(|h| (h.slot, h.distance))
        .collect()
}

#[test]
fn the_grid_search_equals_a_linear_scan_across_seeded_scenes() {
    for seed in 0..50u64 {
        let mut rng = Rng(seed + 1);
        let count = 300 + (rng.next() % 4700) as usize;
        let mut app = app();
        let object = spawn_object(&mut app);
        let mut slots: Vec<(Entity, Vec3)> = Vec::with_capacity(count);
        for _ in 0..count {
            let pos = rng.point(100.0);
            slots.push((spawn_slot(&mut app, object, "sit", pos), pos));
        }
        app.update();
        let index = app.world().resource::<SmartObjectIndex>();
        assert!(index.has_grid(), "seed {seed}: {count} slots should use the grid");
        for _ in 0..4 {
            let center = rng.point(100.0);
            let sphere = QueryRegion::Sphere {
                center,
                radius: 2.0 + rng.unit() * 30.0,
            };
            let lo = rng.point(80.0);
            let aabb = QueryRegion::Aabb {
                min: lo,
                max: lo + rng.point(40.0),
            };
            for region in [sphere, aabb] {
                assert_eq!(
                    hits_of(&app, region),
                    brute_force(&app, &slots, region),
                    "seed {seed}, {region:?}"
                );
            }
        }
        // A small query visits few cells.
        let (candidates, visited) =
            app.world()
                .resource::<SmartObjectIndex>()
                .candidates(&QueryRegion::Sphere {
                    center: Vec3::splat(50.0),
                    radius: 3.0,
                });
        assert!(visited <= 8, "seed {seed}: visited {visited} cells");
        assert!(candidates.len() < count);

        // A moved slot shows next frame, a removed one disappears.
        let (moved, _) = slots[0];
        let new_pos = Vec3::new(500.0, 500.0, 500.0);
        app.world_mut()
            .get_mut::<GlobalTransform>(moved)
            .expect("transform")
            .0 = Affine3A::from_translation(new_pos);
        let (gone, _) = slots[1];
        app.world_mut().despawn(gone);
        slots[0].1 = new_pos;
        slots.remove(1);
        app.update();
        let everything = QueryRegion::Aabb {
            min: Vec3::splat(-10.0),
            max: Vec3::splat(1000.0),
        };
        assert_eq!(
            hits_of(&app, everything),
            brute_force(&app, &slots, everything),
            "seed {seed} after the move"
        );
    }
}

#[test]
fn a_small_world_keeps_the_linear_scan() {
    let mut app = app();
    let object = spawn_object(&mut app);
    for i in 0..10 {
        spawn_slot(&mut app, object, "sit", Vec3::new(i as f32, 0.0, 0.0));
    }
    app.update();
    let index = app.world().resource::<SmartObjectIndex>();
    assert!(!index.has_grid());
    let (candidates, visited) = index.candidates(&QueryRegion::Sphere {
        center: Vec3::ZERO,
        radius: 1.0,
    });
    assert_eq!((candidates.len(), visited), (10, 0));
}

/// Finding 6 (audit G): at equal distance the hit order fell back to the slot's entity bits, which
/// two runs of the same level can allocate differently. The tie goes to the earlier arrival.
#[test]
fn equidistant_slots_follow_spawn_order() {
    let mut app = app();
    let object = spawn_object(&mut app);
    // Holders get ids in spawn order; the lower-id one receives its slot FIRST, so the earlier
    // arrival has the lower id. (The index used to order by `Entity::to_bits`, which in this ECS
    // runs opposite to the index, so this is the arrangement where the two orders disagree.)
    // Both slots are exactly one unit from the origin.
    let a = app.world_mut().spawn_empty().id();
    let b = app.world_mut().spawn_empty().id();
    for (holder, position) in [(a, Vec3::new(-1.0, 0.0, 0.0)), (b, Vec3::new(1.0, 0.0, 0.0))] {
        app.world_mut().entity_mut(holder).insert((
            SmartObjectSlot::new("sit"),
            GlobalTransform(Affine3A::from_translation(position)),
            ChildOf(object),
        ));
    }
    app.update();
    let q = SlotQuery::new(everywhere());
    let first = app
        .world()
        .resource::<SmartObjectIndex>()
        .find_first(app.world(), &q)
        .expect("a hit");
    assert_eq!(
        first.slot, a,
        "the earlier arrival wins the tie, not the entity-key order {b}"
    );
    let near = app
        .world()
        .resource::<SmartObjectIndex>()
        .nearest_available(Vec3::ZERO, "sit")
        .expect("a slot");
    assert_eq!(near.entity, a, "nearest_available agrees with find_first");
}

// ---- error-path regressions (re-landed lost work, 2026-11-09) ----

#[test]
fn a_definition_reload_that_despawns_a_child_object_does_not_panic() {
    let mut app = app();
    let object = app
        .world_mut()
        .spawn(SmartObjectDefinition(def_of(TWO_SLOTS)))
        .id();
    app.update();
    let right = slot_by_def(&app, object, 1).expect("right");
    // The reload drops "right" while "right" itself carries a new, unapplied definition: the
    // parent materializes first (lower index), despawns the child, and the child's turn must skip.
    app.world_mut()
        .entity_mut(right)
        .insert(SmartObjectDefinition(def_of(TWO_SLOTS)));
    let next = def_of(r#"(format_version: 1, slots: [(name: "left")])"#);
    app.world_mut()
        .entity_mut(object)
        .insert(SmartObjectDefinition(next));
    app.update();
    assert!(app.world().get_entity(right).is_err(), "the removed slot is gone");
    assert!(slot_by_def(&app, object, 0).is_some(), "the kept slot survives");
}

#[test]
fn a_child_of_cycle_ends_the_ancestor_walk_and_the_index_rebuild() {
    let mut app = app();
    let a = app.world_mut().spawn(SmartObjectSlot::new("sit")).id();
    let b = app.world_mut().spawn_empty().id();
    app.world_mut().entity_mut(a).insert(ChildOf(b));
    app.world_mut().entity_mut(b).insert(ChildOf(a));
    assert_eq!(
        regia_ai::smart_object::smart_object_ancestor(app.world(), a),
        None
    );
    app.update();
    assert!(app.world().get_entity(a).is_ok());
}

#[test]
fn begin_use_refuses_a_slot_disabled_after_the_claim() {
    let (mut app, _object, slot, a, _b) = one_slot();
    let handle = claim_with(app.world_mut(), slot, a, ClaimPriority::Normal).expect("claims");
    set_enabled(app.world_mut(), slot, false, "test");
    assert_eq!(
        begin_use(app.world_mut(), handle),
        Err(SmartObjectError::SlotDisabled(slot))
    );
    let in_use = app.world().get::<SmartObjectSlot>(slot).is_some_and(|s| s.in_use);
    assert!(!in_use, "the refused use did not start");
}

/// A slot with no local `Transform` is placed by `GlobalTransform`. A fixed step must see a
/// direct write to that global, or the next claim uses the pose from the previous frame.
#[test]
fn audfix_fixed_step_index_sees_a_global_transform_only_move() {
    let mut app = app();
    let object = spawn_object(&mut app);
    let slot = spawn_slot(&mut app, object, "sit", Vec3::ZERO);
    app.update();
    app.world_mut().run_schedule(FixedUpdate);
    let parked = app
        .world()
        .resource::<SmartObjectIndex>()
        .slot(slot)
        .expect("indexed")
        .position;
    assert_eq!(parked, Vec3::ZERO);
    let moved = Vec3::new(4.0, 0.0, 0.0);
    app.world_mut()
        .get_mut::<GlobalTransform>(slot)
        .expect("global")
        .0 = Affine3A::from_translation(moved);
    app.world_mut().run_schedule(FixedUpdate);
    let position = app
        .world()
        .resource::<SmartObjectIndex>()
        .slot(slot)
        .expect("indexed")
        .position;
    assert_eq!(position, moved);
}

/// fixrev-05-k (audit-kimirev-fix-ai-determinism-muse, High 1): with the grid active, a plain
/// search visited cells in cell-coordinate order while a logged search walked the slot list, so
/// an exact distance tie had different winners with and without a rejection log. Every path now
/// ties on the slot's ordinal in the index (role, then arrival).
#[test]
fn grid_ties_agree_with_and_without_a_log() {
    let mut app = app();
    let object = spawn_object(&mut app);
    // Push the slot count past GRID_MIN_SLOTS so the grid answers the query; park the filler
    // slots far outside the region.
    for i in 0..300 {
        spawn_slot(&mut app, object, "far", Vec3::new(1000.0 + i as f32, 500.0, 0.0));
    }
    let a = app.world_mut().spawn_empty().id();
    let b = app.world_mut().spawn_empty().id();
    // `a` arrives first (the smaller ordinal) but sits in the higher cell coordinate, so the
    // old cell-order visitation met `b` first.
    for (holder, position) in [(a, Vec3::new(1.0, 0.0, 0.0)), (b, Vec3::new(-1.0, 0.0, 0.0))] {
        app.world_mut().entity_mut(holder).insert((
            SmartObjectSlot::new("sit"),
            GlobalTransform(Affine3A::from_translation(position)),
            ChildOf(object),
        ));
    }
    app.update();
    let index = app.world().resource::<SmartObjectIndex>();
    assert!(index.has_grid(), "302 slots should activate the grid");
    let q = SlotQuery::new(QueryRegion::Sphere {
        center: Vec3::ZERO,
        radius: 2.0,
    });
    let plain: Vec<_> = index
        .search(app.world(), &q, None)
        .into_iter()
        .map(|h| h.slot)
        .collect();
    let logged: Vec<_> = index
        .search(app.world(), &q, Some(&mut RejectionLog::default()))
        .into_iter()
        .map(|h| h.slot)
        .collect();
    assert_eq!(
        plain,
        [a, b],
        "plain search ties on the index order, not the cell order {b:?}"
    );
    assert_eq!(logged, plain, "a rejection log must not change the winner");
    let near = index.nearest_available(Vec3::ZERO, "sit").expect("a slot");
    assert_eq!(near.entity, a, "nearest_available agrees with both searches");
}

// ---- conformance audit (spec 39 §§ 3, 4, 10; lane spec-17-39, ADR-5636) ---------------------

/// Spec 39 § 3: a tag not declared in the project tag table is dropped with a `tag_unknown`
/// warning at load, and the load continues (the slots still materialize with the declared tags).
#[test]
fn an_undeclared_tag_is_dropped_at_load_and_the_load_continues() {
    let mut app = app();
    let mut registry = regia_framework::tags::TagRegistry::new();
    registry.declare("Activity.Sit", "sitting").expect("declares");
    app.world_mut().insert_resource(registry);
    let def = def_of(
        r#"(format_version: 1,
            activity: ["Activity.Sit", "Activity.Undeclared"],
            slots: [(name: "left", activity: ["Activity.Ghost"], initial_tags: ["Activity.Sit", "Nope.Tag"])])"#,
    );
    let object = app.world_mut().spawn(SmartObjectDefinition(def)).id();
    app.update();
    let rules = app
        .world()
        .get::<SmartObjectRules>(object)
        .expect("the definition applied");
    assert!(rules.activity.has_exact(tag("Activity.Sit")));
    assert!(!rules.activity.has_exact(tag("Activity.Undeclared")));
    let left = slot_by_def(&app, object, 0).expect("the slot still materialized");
    let slot = app.world().get::<SmartObjectSlot>(left).expect("slot");
    assert!(
        slot.activity.is_empty(),
        "the only slot activity tag was undeclared"
    );
    assert!(slot.runtime_tags.has_exact(tag("Activity.Sit")));
    assert!(!slot.runtime_tags.has_exact(tag("Nope.Tag")));
}

/// Spec 39 § 4: a materialized slot is named `<object>/slot_<name>`.
#[test]
fn materialized_slots_are_named_after_their_object() {
    use regia_scene::components::Name;
    let mut app = app();
    let object = app
        .world_mut()
        .spawn((SmartObjectDefinition(def_of(TWO_SLOTS)), Name::new("bench")))
        .id();
    app.update();
    let name_of = |slot: Entity| {
        app.world()
            .get::<Name>(slot)
            .map(|n| n.as_str().to_string())
            .expect("a named slot")
    };
    let left = slot_by_def(&app, object, 0).expect("left");
    let right = slot_by_def(&app, object, 1).expect("right");
    assert_eq!(name_of(left), "bench/slot_left");
    assert_eq!(name_of(right), "bench/slot_right");
}

/// Spec 39 § 4: the derived state puts `Disabled` above `InUse` and `Claimed`; a slot that is
/// re-enabled before the next `PostUpdate` reads its claim state again.
#[test]
fn disabled_outranks_in_use_in_the_derived_state() {
    let (mut app, _object, slot, a, _b) = one_slot();
    let handle = claim_with(app.world_mut(), slot, a, ClaimPriority::Normal).expect("claims");
    begin_use(app.world_mut(), handle).expect("uses");
    set_enabled(app.world_mut(), slot, false, "audit").expect("a slot");
    assert_eq!(slot_state(app.world(), slot), Some(SlotState::Disabled));
    set_enabled(app.world_mut(), slot, true, "audit").expect("a slot");
    assert_eq!(slot_state(app.world(), slot), Some(SlotState::InUse));
}

/// Spec 39 § 10: the overlay colors a claimed slot's ring by priority, so the five levels must be
/// told apart.
#[test]
fn each_claim_priority_has_its_own_overlay_color() {
    let colors: Vec<_> = ClaimPriority::ALL
        .into_iter()
        .map(regia_ai::smart_object::priority_color)
        .collect();
    for (i, a) in colors.iter().enumerate() {
        for b in &colors[i + 1..] {
            assert_ne!(a, b, "two priority levels share an overlay color");
        }
    }
}
