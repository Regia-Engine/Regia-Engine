// SPDX-License-Identifier: MPL-2.0
//! Test-quality sweep 2 (lane test-quality-sweep-2): deterministic tests for public
//! `regia_framework` functions no test referenced before. Pure inputs, no clocks, no files.

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec2, Vec3, Vec4};
use regia_framework::abilities::{ModOp, sample_points, valid_attribute_name};
use regia_framework::camera::nodes::{NodeState, smoothing_factor};
use regia_framework::chooser::{
    Cell, ChooserValue, Column, ColumnKind, ResultEntry, ResultType, ScalarKind, Schema, ValueKind, cell_ron,
    parse_cell, parse_column_kind, parse_result, parse_value, result_ron, value_ron,
};
use regia_framework::conversation::ConversationSettings;
use regia_framework::conversation::voice::estimate_us;
use regia_framework::extension::manifest::valid_id;
use regia_framework::functional::assert::{near_f64, near_vec2, near_vec3, near_vec4, quat_angle};
use regia_framework::functional::runner::parse_setting_value;
use regia_framework::ground_shock::{AIR_SPEED_M_S, GroundShock, P_WAVE_SPEED_M_S};
use regia_framework::save_flow::header_from_bytes;
use regia_framework::save_game::{SAVE_FORMAT_VERSION, SaveGame, SavePlugin, SaveSpec};
use regia_framework::sequencer::asset::{AudioSection, ClipSection, SubSection};
use regia_framework::settings::{CalibrationSettings, GraphicsPreset};
use serde_json::Value;

// ---------------------------------------------------------------------------
// functional::assert
// ---------------------------------------------------------------------------

/// `near_f64` is inclusive at the tolerance, fails just past it, and never passes a NaN.
#[test]
fn near_f64_is_inclusive_and_rejects_nan() {
    assert!(near_f64(1.5, 1.0, 0.5, "x").is_ok());
    let err = near_f64(1.5000001, 1.0, 0.5, "x").expect_err("past the tolerance");
    assert!(err.starts_with("x: expected 1"), "{err}");
    assert!(near_f64(f64::NAN, 1.0, 1.0e9, "").is_err());
    assert!(near_f64(1.0, f64::NAN, 1.0e9, "").is_err());
    assert!(
        near_f64(f64::INFINITY, f64::INFINITY, 0.0, "").is_err(),
        "inf - inf is NaN"
    );
    // No label: no `: ` prefix.
    let err = near_f64(3.0, 1.0, 0.0, "").expect_err("far");
    assert!(err.starts_with("expected 1"), "{err}");
}

/// Regression (tqs2 fix): a vector with a NaN component never counts as near (the per-component
/// maximum used `f32::max`, which drops NaN, so `near_vec2(NaN, ..)` passed).
#[test]
fn near_vectors_reject_nan_components() {
    let nan = f32::NAN;
    assert!(near_vec2(Vec2::new(nan, 0.0), Vec2::ZERO, 1.0, "p").is_err());
    assert!(near_vec2(Vec2::ZERO, Vec2::new(0.0, nan), 1.0, "p").is_err());
    assert!(near_vec3(Vec3::new(0.0, nan, 0.0), Vec3::ZERO, 1.0, "p").is_err());
    assert!(near_vec4(Vec4::new(0.0, 0.0, 0.0, nan), Vec4::ZERO, 1.0, "p").is_err());
    // A NaN after a large difference is still caught (order does not matter).
    assert!(near_vec4(Vec4::new(5.0, nan, 0.0, 0.0), Vec4::ZERO, 10.0, "p").is_err());
}

/// `near_vec2` / `near_vec4` compare the worst component, inclusive, and name the kind.
#[test]
fn near_vectors_use_the_worst_component() {
    assert!(near_vec2(Vec2::new(1.0, 2.0), Vec2::new(1.25, 1.75), 0.25, "").is_ok());
    let err = near_vec2(Vec2::new(1.0, 2.0), Vec2::new(1.0, 2.5), 0.25, "pos").expect_err("y off");
    assert!(err.starts_with("pos: Vec2 expected"), "{err}");
    assert!(err.contains("largest difference 0.5"), "{err}");
    assert!(near_vec4(Vec4::ONE, Vec4::new(1.0, 1.0, 1.0, 1.5), 0.5, "").is_ok());
    let err = near_vec4(Vec4::ONE, Vec4::new(1.0, 1.0, 1.0, 2.0), 0.5, "").expect_err("w off");
    assert!(err.starts_with("Vec4 expected"), "{err}");
}

/// `quat_angle` is 0 for `q` against `-q`, the rotation angle otherwise, and normalizes inputs.
#[test]
fn quat_angle_measures_rotation_difference() {
    let q = Quat::from_rotation_y(0.7);
    assert!(quat_angle(q, -q).abs() < 1.0e-3, "q and -q are one rotation");
    assert!(quat_angle(q, q).abs() < 1.0e-3);
    let a = quat_angle(Quat::IDENTITY, Quat::from_rotation_x(1.0));
    assert!((a - 1.0).abs() < 1.0e-5, "{a}");
    // Half a turn: π.
    let a = quat_angle(Quat::IDENTITY, Quat::from_rotation_z(std::f32::consts::PI));
    assert!((a - std::f32::consts::PI).abs() < 1.0e-3, "{a}");
    // Unnormalized input is normalized first.
    let scaled = Quat::from_xyzw(q.x * 3.0, q.y * 3.0, q.z * 3.0, q.w * 3.0);
    assert!(quat_angle(scaled, q).abs() < 1.0e-3);
}

// ---------------------------------------------------------------------------
// functional::runner
// ---------------------------------------------------------------------------

/// `parse_setting_value` tries bool, then integer, then float, else keeps the text.
#[test]
fn parse_setting_value_orders_bool_int_float_string() {
    assert_eq!(parse_setting_value("true"), Value::Bool(true));
    assert_eq!(parse_setting_value("false"), Value::Bool(false));
    assert_eq!(parse_setting_value("42"), Value::from(42i64));
    assert_eq!(parse_setting_value("-7"), Value::from(-7i64));
    assert_eq!(parse_setting_value("1.5"), Value::from(1.5f64));
    assert_eq!(parse_setting_value("1e3"), Value::from(1000.0f64));
    assert_eq!(parse_setting_value("High"), Value::String("High".into()));
    // Case matters for bools (`str::parse::<bool>` is exact).
    assert_eq!(parse_setting_value("True"), Value::String("True".into()));
    assert_eq!(parse_setting_value(""), Value::String(String::new()));
}

/// Regression (tqs2 fix): non-finite float spellings stay strings; they used to parse as `f64` and
/// turn into JSON `null` (serde_json has no NaN or infinity), losing the override.
#[test]
fn parse_setting_value_keeps_non_finite_spellings_as_text() {
    for text in ["nan", "NaN", "inf", "-inf", "infinity", "Infinity"] {
        assert_eq!(parse_setting_value(text), Value::String(text.into()), "{text}");
    }
}

// ---------------------------------------------------------------------------
// abilities
// ---------------------------------------------------------------------------

/// `sample_points`: `None` when empty, clamped outside, linear inside, and a zero-width span steps.
#[test]
fn sample_points_is_clamped_piecewise_linear() {
    assert_eq!(sample_points(&[], 1.0), None);
    let pts = [(0.0, 10.0), (2.0, 20.0), (4.0, 0.0)];
    assert_eq!(sample_points(&pts, -5.0), Some(10.0));
    assert_eq!(sample_points(&pts, 0.0), Some(10.0));
    assert_eq!(sample_points(&pts, 1.0), Some(15.0));
    assert_eq!(sample_points(&pts, 2.0), Some(20.0));
    assert_eq!(sample_points(&pts, 3.0), Some(10.0));
    assert_eq!(sample_points(&pts, 9.0), Some(0.0));
    assert_eq!(sample_points(&[(1.0, 7.0)], 100.0), Some(7.0));
    // A duplicated x (unsorted data a validator would flag) takes the later value, no division.
    let step = [(0.0, 1.0), (1.0, 2.0), (1.0, 5.0), (2.0, 5.0)];
    assert_eq!(sample_points(&step, 1.0), Some(2.0));
    assert_eq!(sample_points(&step, 1.5), Some(5.0));
}

/// `ModOp::apply_to_base`: adds add, percent scales by `1 + amount`, scale multiplies, replace sets.
#[test]
fn mod_op_apply_to_base_matches_each_phase() {
    assert_eq!(ModOp::AddPre.apply_to_base(10.0, 5.0), 15.0);
    assert_eq!(ModOp::AddPost.apply_to_base(10.0, -15.0), -5.0);
    assert_eq!(ModOp::Percent.apply_to_base(10.0, 0.5), 15.0);
    assert_eq!(ModOp::Percent.apply_to_base(10.0, -1.0), 0.0);
    assert_eq!(ModOp::Scale.apply_to_base(10.0, 0.25), 2.5);
    assert_eq!(ModOp::Replace { priority: 3 }.apply_to_base(10.0, 42.0), 42.0);
    assert_eq!(ModOp::Replace { priority: -1 }.apply_to_base(f32::NAN, 1.0), 1.0);
}

/// `valid_attribute_name` accepts `[a-z][a-z0-9_]{0,47}` and nothing else.
#[test]
fn valid_attribute_name_boundaries() {
    assert!(valid_attribute_name("h"));
    assert!(valid_attribute_name("health_max_2"));
    assert!(valid_attribute_name(&format!("a{}", "b".repeat(47))));
    assert!(!valid_attribute_name(&format!("a{}", "b".repeat(48))), "49 bytes");
    assert!(!valid_attribute_name(""));
    assert!(!valid_attribute_name("_health"));
    assert!(!valid_attribute_name("2health"));
    assert!(!valid_attribute_name("Health"));
    assert!(!valid_attribute_name("health-max"));
    assert!(!valid_attribute_name("health.max"));
    assert!(!valid_attribute_name("héalth"));
}

// ---------------------------------------------------------------------------
// extension::manifest
// ---------------------------------------------------------------------------

/// `valid_id` accepts `[a-z][a-z0-9_]{1,31}`: two to 32 bytes, lowercase first.
#[test]
fn valid_extension_id_boundaries() {
    assert!(!valid_id("a"), "one byte is too short");
    assert!(valid_id("ab"));
    assert!(valid_id("my_ext_2"));
    assert!(valid_id(&format!("a{}", "z".repeat(31))));
    assert!(!valid_id(&format!("a{}", "z".repeat(32))), "33 bytes");
    assert!(!valid_id(""));
    assert!(!valid_id("1ab"));
    assert!(!valid_id("_ab"));
    assert!(!valid_id("Ab"));
    assert!(!valid_id("a-b"));
    assert!(!valid_id("a b"));
}

// ---------------------------------------------------------------------------
// camera::nodes
// ---------------------------------------------------------------------------

/// `smoothing_factor` is `1 − e^(−k·dt)` in `[0, 1]`, and 0 for bad input.
#[test]
fn smoothing_factor_range_and_bad_input() {
    assert_eq!(smoothing_factor(0.0, 1.0), 0.0);
    assert_eq!(smoothing_factor(5.0, 0.0), 0.0);
    let t = smoothing_factor(1.0, 1.0);
    assert!((t - (1.0 - (-1.0f32).exp())).abs() < 1.0e-6, "{t}");
    // Two half steps cover the same share as one whole step.
    let half = smoothing_factor(3.0, 0.05);
    let whole = smoothing_factor(3.0, 0.1);
    assert!((1.0 - (1.0 - half) * (1.0 - half) - whole).abs() < 1.0e-5);
    assert_eq!(
        smoothing_factor(f32::INFINITY, 0.1),
        1.0,
        "an infinite rate snaps"
    );
    assert_eq!(smoothing_factor(-1.0, 1.0), 0.0, "negative rate clamps to 0");
    assert_eq!(smoothing_factor(1.0, -1.0), 0.0, "negative step clamps to 0");
    assert_eq!(smoothing_factor(f32::NAN, 1.0), 0.0);
    assert_eq!(
        smoothing_factor(f32::INFINITY, 0.0),
        0.0,
        "inf·0 is NaN: bad input"
    );
}

/// `NodeState::lerp` mixes position and scalars, ORs `init`, and keeps self's hit and warning.
#[test]
fn node_state_lerp_mixes_state_and_keeps_flags() {
    let a = NodeState {
        init: false,
        v: Vec3::ZERO,
        f: [0.0, 10.0, -2.0],
        last_hit: 3.0,
        warned: true,
    };
    let b = NodeState {
        init: true,
        v: Vec3::new(2.0, 4.0, -8.0),
        f: [1.0, 20.0, 2.0],
        last_hit: f32::NAN,
        warned: false,
    };
    let m = a.lerp(b, 0.5);
    assert!(m.init);
    assert_eq!(m.v, Vec3::new(1.0, 2.0, -4.0));
    assert_eq!(m.f, [0.5, 15.0, 0.0]);
    assert_eq!(m.last_hit, 3.0);
    assert!(m.warned);
    assert_eq!(a.lerp(b, 0.0).v, a.v);
    assert_eq!(a.lerp(b, 1.0).f, b.f);
}

// ---------------------------------------------------------------------------
// ground_shock
// ---------------------------------------------------------------------------

/// The arrival delays are distance over speed, 0 at or before the source, and fall back to the
/// physical default for a bad speed.
#[test]
fn ground_shock_delays_and_speed_fallback() {
    let shock = GroundShock::default();
    assert_eq!(shock.air_delay_f64(343.0), 343.0 / f64::from(AIR_SPEED_M_S));
    assert_eq!(
        shock.seismic_delay_f64(5500.0),
        5500.0 / f64::from(P_WAVE_SPEED_M_S)
    );
    assert!(
        shock.seismic_delay_f64(100.0) < shock.air_delay_f64(100.0),
        "ground wave first"
    );
    assert_eq!(shock.air_delay_f64(0.0), 0.0);
    assert_eq!(shock.air_delay_f64(-10.0), 0.0);
    assert_eq!(shock.air_delay_f64(f32::NAN), 0.0);
    assert_eq!(
        f64::from(shock.air_delay_s(686.0)),
        shock.air_delay_f64(686.0) as f32 as f64
    );
    for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        let s = GroundShock {
            p_speed_m_s: bad,
            air_speed_m_s: bad,
            ..GroundShock::default()
        };
        assert_eq!(
            s.air_delay_f64(343.0),
            shock.air_delay_f64(343.0),
            "air speed {bad}"
        );
        assert_eq!(
            s.seismic_delay_f64(55.0),
            shock.seismic_delay_f64(55.0),
            "p speed {bad}"
        );
    }
    let fast = GroundShock {
        air_speed_m_s: 686.0,
        ..GroundShock::default()
    };
    assert_eq!(fast.air_delay_f64(343.0), 0.5);
}

// ---------------------------------------------------------------------------
// chooser format
// ---------------------------------------------------------------------------

/// `parse_value` reads what `value_ron` writes for every scalar kind, bit-exact for floats.
#[test]
fn chooser_value_ron_round_trips() {
    let cases = [
        (ValueKind::Bool, ChooserValue::Bool(true)),
        (ValueKind::Int, ChooserValue::Int(i64::MIN)),
        (ValueKind::Int, ChooserValue::Int(i64::MAX)),
        (ValueKind::Float, ChooserValue::Float(0.1)),
        (ValueKind::Float, ChooserValue::Float(-0.0)),
        (ValueKind::Float, ChooserValue::Float(f32::MAX)),
        (ValueKind::Float, ChooserValue::Float(1.0e-30)),
        (ValueKind::Name, ChooserValue::Name("say \"hi\" \\ ü".into())),
        (ValueKind::Asset, ChooserValue::Asset("anim/run.clip.ron".into())),
        (
            ValueKind::Enum(vec!["Walk".into(), "Run".into()]),
            ChooserValue::Enum("Run".into()),
        ),
    ];
    for (kind, value) in cases {
        let text = value_ron(&value);
        let back = parse_value(&kind, &text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert!(back.identical(&value), "{text}: {back:?} != {value:?}");
    }
}

/// `parse_value` rejects NaN, undeclared enum variants and text of the wrong kind.
#[test]
fn chooser_parse_value_rejects_bad_text() {
    assert!(parse_value(&ValueKind::Float, "NaN").is_err());
    let err = parse_value(&ValueKind::Enum(vec!["Walk".into()]), "\"Fly\"").expect_err("undeclared");
    assert!(err.contains("not one of the declared variants"), "{err}");
    assert!(parse_value(&ValueKind::Int, "1.5").is_err());
    assert!(parse_value(&ValueKind::Bool, "1").is_err());
    assert!(parse_value(&ValueKind::Name, "bare").is_err(), "names are quoted");
    assert_eq!(
        parse_value(&ValueKind::Int, "  7 ").ok(),
        Some(ChooserValue::Int(7)),
        "trimmed"
    );
}

fn distance_kind() -> ColumnKind {
    ColumnKind::FloatDistance {
        influence: 1.0,
        max_distance: 10.0,
        wrap: None,
        drop_beyond: false,
    }
}

/// `parse_cell` reads what `cell_ron` writes, per column kind, including `Any`.
#[test]
fn chooser_cell_ron_round_trips() {
    let schema = Schema::default();
    let cases = [
        (ColumnKind::Bool, Cell::Any),
        (ColumnKind::Bool, Cell::Bool(true)),
        (ColumnKind::Bool, Cell::Bool(false)),
        (ColumnKind::IntRange, Cell::IntRange(-3, 9)),
        (ColumnKind::IntRange, Cell::IntRange(9, -3)),
        (ColumnKind::IntRange, Cell::Any),
        (ColumnKind::FloatRange, Cell::FloatRange(0.1, 2.5)),
        (ColumnKind::Enum, Cell::Is("Run".into())),
        (ColumnKind::Enum, Cell::IsNot("Run".into())),
        (ColumnKind::Enum, Cell::OneOf(vec!["A".into(), "B\"".into()])),
        (ColumnKind::Asset, Cell::Kind("clip".into())),
        (ColumnKind::Asset, Cell::Is("a/b.clip.ron".into())),
        (ColumnKind::Name, Cell::Name("Any".into())),
        (ColumnKind::Name, Cell::Name("loot_a".into())),
        (distance_kind(), Cell::Target(3.25)),
        (ColumnKind::Weight, Cell::Weight(0.5)),
        (ColumnKind::SetFloat, Cell::Value(ChooserValue::Float(1.5))),
        (ColumnKind::SetInt, Cell::Value(ChooserValue::Int(-4))),
        (ColumnKind::SetName, Cell::Value(ChooserValue::Name("x".into()))),
        (ColumnKind::SetEnum, Cell::Value(ChooserValue::Enum("Any".into()))),
    ];
    for (kind, cell) in cases {
        let column = Column::new("c", kind.clone());
        let text = cell_ron(&cell);
        let back = parse_cell(&column, &schema, &text).unwrap_or_else(|e| panic!("{kind:?} `{text}`: {e}"));
        assert_eq!(back, cell, "{kind:?} `{text}`");
    }
}

/// A quoted `"Any"` in a `Name` column is the literal name, the bare word is the wildcard; a
/// cell of the wrong form for its column is an error.
#[test]
fn chooser_parse_cell_distinguishes_any_and_rejects_wrong_forms() {
    let schema = Schema::default();
    let name = Column::new("n", ColumnKind::Name);
    assert_eq!(parse_cell(&name, &schema, "Any"), Ok(Cell::Any));
    assert_eq!(
        parse_cell(&name, &schema, "\"Any\""),
        Ok(Cell::Name("Any".into()))
    );
    let range = Column::new("r", ColumnKind::IntRange);
    assert!(parse_cell(&range, &schema, "(1.5, 2)").is_err());
    assert!(parse_cell(&range, &schema, "5").is_err());
    let flag = Column::new("b", ColumnKind::Bool);
    assert!(
        parse_cell(&flag, &schema, "true").is_err(),
        "bool cells are `True` / `False`"
    );
    let set_enum_declared = {
        let mut s = Schema::default();
        s.outputs
            .insert("gait".into(), ValueKind::Enum(vec!["Walk".into()]));
        s
    };
    let mut emit = Column::new("e", ColumnKind::SetEnum);
    emit.output = Some("gait".into());
    assert!(parse_cell(&emit, &set_enum_declared, "\"Walk\"").is_ok());
    assert!(
        parse_cell(&emit, &set_enum_declared, "\"Fly\"").is_err(),
        "undeclared variant"
    );
}

/// `parse_column_kind` reads unit and struct kinds and rejects unknown names.
#[test]
fn chooser_parse_column_kind_forms() {
    assert_eq!(parse_column_kind("Bool"), Ok(ColumnKind::Bool));
    assert_eq!(parse_column_kind(" SetFloat "), Ok(ColumnKind::SetFloat));
    assert_eq!(
        parse_column_kind("FloatDistance(max_distance: 10.0)"),
        Ok(distance_kind()),
        "serde defaults fill the optional fields"
    );
    assert!(parse_column_kind("Nope").is_err());
    assert!(parse_column_kind("bool").is_err(), "names are case-sensitive");
    for kind in [ColumnKind::Weight, ColumnKind::SetTags, ColumnKind::Enum] {
        assert_eq!(parse_column_kind(kind.ron_name()).as_ref(), Ok(&kind));
    }
}

/// `parse_result` reads what `result_ron` writes, and types a `Value(..)` against the table.
#[test]
fn chooser_result_ron_round_trips_and_types_values() {
    let float = ResultType::Value(ScalarKind::Float);
    let asset = ResultType::Asset("clip".into());
    for (ty, entry) in [
        (&float, ResultEntry::Value(ChooserValue::Float(2.5))),
        (&asset, ResultEntry::Asset("anim/a.clip.ron".into())),
        (&asset, ResultEntry::Chooser("sub.chooser.ron".into())),
        (&ResultType::OutputOnly, ResultEntry::Nothing),
    ] {
        let text = result_ron(&entry);
        let back = parse_result(ty, &text).unwrap_or_else(|e| panic!("`{text}`: {e:?}"));
        assert_eq!(back, entry, "`{text}`");
    }
    let issues = parse_result(&asset, "Value(1.0)").expect_err("not a value table");
    assert_eq!(issues[0].code, "chooser_result_type_mismatch");
    let issues = parse_result(&float, "Value(true)").expect_err("wrong scalar");
    assert_eq!(issues[0].code, "chooser_result_type_mismatch");
    let issues = parse_result(&float, "Garbage(").expect_err("syntax");
    assert_eq!(issues[0].code, "chooser_bad_file");
}

// ---------------------------------------------------------------------------
// sequencer sections
// ---------------------------------------------------------------------------

/// `ClipSection::weight_at` ramps linearly in and out, and a blend out over a blend in of the
/// same length sums to the weight.
#[test]
fn clip_section_weight_ramps() {
    let clip = |start: f32, end: f32, blend_in: f32, blend_out: f32| ClipSection {
        clip: "c".into(),
        start,
        end,
        offset: 0.0,
        speed: 1.0,
        looping: false,
        blend_in,
        blend_out,
        weight: 0.5,
    };
    let s = clip(0.0, 4.0, 1.0, 1.0);
    assert_eq!(s.weight_at(0.0), 0.0);
    assert_eq!(s.weight_at(0.5), 0.25);
    assert_eq!(s.weight_at(2.0), 0.5);
    assert_eq!(s.weight_at(3.5), 0.25);
    assert_eq!(s.weight_at(4.0), 0.0);
    assert_eq!(
        clip(0.0, 4.0, 0.0, 0.0).weight_at(0.0),
        0.5,
        "no blend: full at once"
    );
    // Crossfade: out of [0, 4) over its last second, into [3, 7) over its first.
    let a = clip(0.0, 4.0, 0.0, 1.0);
    let b = clip(3.0, 7.0, 1.0, 0.0);
    for i in 0..=8 {
        let t = 3.0 + f64::from(i) / 8.0;
        assert!((a.weight_at(t) + b.weight_at(t) - 0.5).abs() < 1.0e-6, "t {t}");
    }
}

/// `AudioSection::fade_at` is the same ramp without a weight: 1 with no fades, 0 at the edges.
#[test]
fn audio_section_fade_ramps() {
    let audio = |fade_in: f32, fade_out: f32| AudioSection {
        sound: "s".into(),
        start: 1.0,
        end: 3.0,
        offset: 0.0,
        volume_db: 0.0,
        fade_in,
        fade_out,
        bus: None,
        ..AudioSection::new("s", 1.0, 3.0)
    };
    assert_eq!(audio(0.0, 0.0).fade_at(1.0), 1.0);
    assert_eq!(audio(0.0, 0.0).fade_at(3.0), 1.0);
    let f = audio(1.0, 0.5);
    assert_eq!(f.fade_at(1.0), 0.0);
    assert_eq!(f.fade_at(1.5), 0.5);
    assert_eq!(f.fade_at(2.25), 1.0);
    assert_eq!(f.fade_at(2.75), 0.5);
    assert_eq!(f.fade_at(3.0), 0.0);
    // Fades longer than the section meet in the middle.
    assert_eq!(audio(2.0, 2.0).fade_at(2.0), 0.5);
}

/// `SubSection::parent_time` inverts `local_time` for any offset and speed.
#[test]
fn sub_section_parent_time_inverts_local_time() {
    for (offset, speed) in [(0.0, 1.0), (2.5, 2.0), (-1.0, 0.25), (10.0, 0.5)] {
        let sub = SubSection {
            sequence: "s".into(),
            start: 4.0,
            end: 12.0,
            offset,
            speed,
            bindings: Default::default(),
            ..SubSection::new("s", 4.0, 12.0)
        };
        assert_eq!(sub.local_time(4.0), f64::from(offset));
        for t in [4.0, 5.5, 8.0, 12.0] {
            let back = sub.parent_time(sub.local_time(t));
            assert!(
                (back - t).abs() < 1.0e-9,
                "offset {offset} speed {speed}: {t} -> {back}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// conversation
// ---------------------------------------------------------------------------

/// `estimate_us` rounds graphemes per second up, floors at the minimum display time, and treats a
/// zero rate as one per second.
#[test]
fn conversation_estimate_rounds_up_and_floors() {
    let settings = ConversationSettings {
        estimate_cps: 10,
        min_display_ms: 500,
        ..ConversationSettings::default()
    };
    assert_eq!(estimate_us("", &settings), 500_000);
    assert_eq!(estimate_us("abc", &settings), 1_000_000, "ceil(3/10) = 1 s");
    assert_eq!(estimate_us(&"a".repeat(10), &settings), 1_000_000);
    assert_eq!(estimate_us(&"a".repeat(11), &settings), 2_000_000);
    let floor = ConversationSettings {
        min_display_ms: 2500,
        ..settings.clone()
    };
    assert_eq!(estimate_us(&"a".repeat(11), &floor), 2_500_000);
    let zero = ConversationSettings {
        estimate_cps: 0,
        min_display_ms: 0,
        ..settings
    };
    assert_eq!(estimate_us("abcd", &zero), 4_000_000);
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

/// `GraphicsPreset::from_tier` maps mobile tiers to their desktop names, and `tier` maps back to
/// the same tier on the platform it came from.
#[test]
fn graphics_preset_from_tier_round_trips() {
    use regia_render::QualityTier as T;
    assert_eq!(GraphicsPreset::from_tier(T::MobileLow), GraphicsPreset::Low);
    assert_eq!(GraphicsPreset::from_tier(T::MobileHigh), GraphicsPreset::High);
    for tier in T::ALL {
        let android = matches!(tier, T::MobileLow | T::MobileHigh);
        assert_eq!(GraphicsPreset::from_tier(tier).tier(android), tier, "{tier:?}");
    }
    assert_eq!(GraphicsPreset::Clean.tier(false), T::High);
}

/// `CalibrationSettings::from_calibration` keeps a non-default peak explicitly and the default
/// as "use the display"; `to_calibration` restores it and clamps to the player ranges.
#[test]
fn calibration_settings_round_trip_and_clamp() {
    use regia_render::DisplayCalibration;
    use regia_render::calibration::{DEFAULT_PEAK_NITS, PEAK_RANGE};
    let neutral = CalibrationSettings::from_calibration(&DisplayCalibration::NEUTRAL, true);
    assert!(neutral.hdr);
    assert_eq!(neutral.peak_nits, None);
    assert_eq!(neutral.to_calibration(None), DisplayCalibration::NEUTRAL);
    let custom = DisplayCalibration {
        peak_nits: 600.0,
        black_level: 0.25,
        ..DisplayCalibration::NEUTRAL
    };
    let saved = CalibrationSettings::from_calibration(&custom, false);
    assert_eq!(saved.peak_nits, Some(600.0));
    assert_eq!(saved.to_calibration(None), custom);
    let wild = CalibrationSettings {
        peak_nits: Some(1.0e9),
        ..saved
    };
    assert_eq!(wild.to_calibration(None).peak_nits, PEAK_RANGE.1);
    let broken = CalibrationSettings {
        peak_nits: Some(f32::NAN),
        ..neutral
    };
    assert_eq!(broken.to_calibration(None).peak_nits, DEFAULT_PEAK_NITS);
}

// ---------------------------------------------------------------------------
// save_flow
// ---------------------------------------------------------------------------

/// `header_from_bytes` reads a real save's header, and rejects short, mis-tagged or damaged bytes.
#[test]
fn save_header_from_bytes_accepts_real_and_rejects_damage() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SavePlugin::default()));
    let bytes = SaveGame::encode(app.world_mut(), &SaveSpec::default()).expect("encode an empty world");
    let header = header_from_bytes(&bytes).expect("a fresh save has a header");
    assert_eq!(header.version, SAVE_FORMAT_VERSION);
    assert!(!header.has_thumbnail);

    assert!(header_from_bytes(&[]).is_none());
    assert!(
        header_from_bytes(&bytes[..11]).is_none(),
        "shorter than magic + version + crc"
    );
    let mut tagged = bytes.clone();
    tagged[0] ^= 0xff;
    assert!(header_from_bytes(&tagged).is_none(), "bad magic");
    for i in [8, bytes.len() / 2, bytes.len() - 5, bytes.len() - 1] {
        let mut flipped = bytes.clone();
        flipped[i] ^= 0x01;
        assert!(header_from_bytes(&flipped).is_none(), "byte {i} flipped");
    }
    let mut truncated = bytes.clone();
    truncated.pop();
    assert!(header_from_bytes(&truncated).is_none());
}
