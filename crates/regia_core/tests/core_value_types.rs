// SPDX-License-Identifier: MPL-2.0
//! Public-API checks for the small value types in `regia_core`: colors, ids, names, type
//! hashes, privacy scrubbing, the PCG32 generator, curves and isometries. Each test pins
//! behaviour the unit tests in `src/` do not: error paths, boundary values and serde forms.

use std::borrow::Cow;
use std::collections::HashMap;

use regia_core::glam::{Quat, Vec3};
use regia_core::id::{Guid, Name, TypePathHash};
use regia_core::math::{Color, ColorParseError, Curve, Interp, Isometry, Keyframe, Rng, inverse_lerp, remap};
use regia_core::privacy::scrub_with;

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-5
}

// ---- Color -------------------------------------------------------------------------------

#[test]
fn hex_with_the_wrong_digit_count_is_a_length_error() {
    for bad in ["", "#", "#fff", "#12345", "#1234567", "#123456789", "12345"] {
        assert!(
            matches!(Color::from_hex(bad), Err(ColorParseError::Length(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn hex_with_a_non_hex_character_is_a_digit_error() {
    for bad in ["#gg0000", "#12345z", "zzzzzzzz", "#ff00 0"] {
        assert!(
            matches!(Color::from_hex(bad), Err(ColorParseError::Digit(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn hex_with_multibyte_characters_is_an_error_not_a_panic() {
    // Six bytes long but only three characters: slicing by byte index must not panic.
    assert!(Color::from_hex("ééé").is_err());
    assert!(Color::from_hex("#éé00").is_err());
}

#[test]
fn hex_error_messages_quote_the_input() {
    let err = Color::from_hex("#12").expect_err("short");
    assert!(err.to_string().contains("#12"), "{err}");
    let err = Color::from_hex("#12345z").expect_err("digit");
    assert!(err.to_string().contains("non-hex"), "{err}");
}

#[test]
fn hex_accepts_upper_case_and_a_missing_hash() {
    assert_eq!(Color::from_hex("FF8000"), Color::from_hex("#ff8000"));
    assert_eq!(
        Color::from_hex("#ff8000").map(|c| c.to_hex()),
        Ok("#ff8000ff".to_owned())
    );
}

#[test]
fn hex_alpha_is_linear_and_not_gamma_decoded() {
    let c = Color::from_hex("#00000080").expect("rgba");
    assert!(close(c.a, 128.0 / 255.0), "{}", c.a);
    assert_eq!(c.to_srgb_u8(), [0, 0, 0, 128]);
    assert_eq!(Color::from_hex("#102030").expect("rgb").a, 1.0);
}

#[test]
fn every_srgb_byte_round_trips_through_linear() {
    for v in 0..=255u8 {
        let c = Color::from_srgb_u8(v, v, v, v);
        assert_eq!(c.to_srgb_u8(), [v, v, v, v], "value {v}");
    }
}

#[test]
fn to_srgb_clamps_colour_but_not_alpha() {
    let [r, g, b, a] = Color::new(2.0, -1.0, 0.0, 3.5).to_srgb();
    // The clamped 1.0 encodes as 1.055 - 0.055, one f32 step under 1.
    assert!((r - 1.0).abs() < 1e-6, "{r}");
    assert_eq!((g, b), (0.0, 0.0));
    assert_eq!(a, 3.5);
    assert_eq!(Color::new(2.0, -1.0, 0.0, 3.5).to_srgb_u8(), [255, 0, 0, 255]);
}

#[test]
fn colour_helpers_keep_components_straight() {
    let c = Color::new(0.25, 0.5, 0.75, 0.5);
    assert_eq!(c.to_array(), [0.25, 0.5, 0.75, 0.5]);
    assert_eq!(c.to_vec4().to_array(), [0.25, 0.5, 0.75, 0.5]);
    assert_eq!(c.to_vec3(), Vec3::new(0.25, 0.5, 0.75));
    assert_eq!(c.with_alpha(1.0), Color::new(0.25, 0.5, 0.75, 1.0));
    let p = c.premultiplied();
    assert_eq!(p, Color::new(0.125, 0.25, 0.375, 0.5));
    assert!(close(Color::WHITE.luminance(), 1.0));
    assert_eq!(Color::BLACK.luminance(), 0.0);
    assert!(Color::GREEN.luminance() > Color::RED.luminance());
    assert!(Color::RED.luminance() > Color::BLUE.luminance());
}

#[test]
fn colour_serde_round_trips_in_json_and_postcard() {
    let c = Color::new(0.1, 0.2, 0.3, 0.4);
    let json = serde_json::to_string(&c).expect("json");
    assert_eq!(serde_json::from_str::<Color>(&json).expect("parse"), c);
    let bytes = postcard::to_allocvec(&c).expect("postcard");
    assert_eq!(postcard::from_bytes::<Color>(&bytes).expect("parse"), c);
    assert_eq!(bytes.len(), 16, "four f32s, no framing");
}

// ---- Guid / Name / TypePathHash -------------------------------------------------------------

const GUID_TEXT: &str = "00112233-4455-6677-8899-aabbccddeeff";

#[test]
fn guid_display_and_parse_round_trip() {
    let guid: Guid = GUID_TEXT.parse().expect("parse");
    assert_eq!(guid.as_u128(), 0x0011_2233_4455_6677_8899_aabb_ccdd_eeff);
    assert_eq!(guid.to_string(), GUID_TEXT);
    assert_eq!(format!("{guid:?}"), format!("Guid({GUID_TEXT})"));
    assert_eq!(Guid::from_u128(guid.as_u128()), guid);
}

#[test]
fn guid_parse_ignores_dash_placement_and_accepts_upper_case() {
    let plain: Guid = "00112233445566778899AABBCCDDEEFF".parse().expect("plain");
    let dashed: Guid = GUID_TEXT.parse().expect("dashed");
    assert_eq!(plain, dashed);
}

#[test]
fn guid_parse_rejects_wrong_length_and_bad_digits() {
    for bad in [
        "",
        "-",
        "0011223344556677889900112233445566",
        "00112233445566778899aabbccddeef",
        "00112233-4455-6677-8899-aabbccddeefg",
        "0011 2233-4455-6677-8899-aabbccddeeff",
        "{00112233-4455-6677-8899-aabbccddeeff}",
    ] {
        let err = bad.parse::<Guid>().expect_err(bad);
        assert_eq!(err.0, bad, "the error keeps the offending text");
    }
}

#[test]
fn guid_nil_and_random() {
    assert!(Guid::NIL.is_nil());
    assert_eq!(Guid::NIL.to_string(), "00000000-0000-0000-0000-000000000000");
    assert_eq!(Guid::default(), Guid::NIL);
    let a = Guid::new_random();
    let b = Guid::new_random();
    assert!(!a.is_nil());
    assert_ne!(a, b);
}

#[test]
fn guid_serializes_as_text_in_json_and_binary_in_postcard() {
    let guid: Guid = GUID_TEXT.parse().expect("parse");
    let json = serde_json::to_string(&guid).expect("json");
    assert_eq!(json, format!("\"{GUID_TEXT}\""));
    assert_eq!(serde_json::from_str::<Guid>(&json).expect("parse"), guid);
    assert!(serde_json::from_str::<Guid>("\"not-a-guid\"").is_err());
    let bytes = postcard::to_allocvec(&guid).expect("postcard");
    assert_eq!(postcard::from_bytes::<Guid>(&bytes).expect("parse"), guid);
    assert!(bytes.len() <= 19, "a varint u128, not the 36-char text");
}

#[test]
fn name_compares_with_str_and_looks_up_by_str() {
    let name = Name::from("hero");
    assert_eq!(name, "hero");
    assert_eq!(name.as_str(), "hero");
    assert_eq!(name.len(), 4, "derefs to str");
    assert_eq!(name.to_string(), "hero");
    assert_eq!(format!("{name:?}"), "\"hero\"");
    assert_eq!(Name::default().as_str(), "");
    assert_eq!(Name::from(String::from("hero")), name);
    assert_eq!(name.clone(), name);

    let mut map: HashMap<Name, u32> = HashMap::new();
    map.insert(name, 7);
    assert_eq!(map.get("hero"), Some(&7), "Borrow<str> makes &str lookups work");
    assert_eq!(map.get("villain"), None);
}

#[test]
fn name_orders_like_its_string_and_round_trips_serde() {
    let mut names = [Name::from("b"), Name::from("a"), Name::from("c")];
    names.sort();
    let sorted: Vec<&str> = names.iter().map(Name::as_str).collect();
    assert_eq!(sorted, ["a", "b", "c"]);
    let json = serde_json::to_string(&names[0]).expect("json");
    assert_eq!(json, "\"a\"");
    assert_eq!(serde_json::from_str::<Name>(&json).expect("parse"), "a");
    let bytes = postcard::to_allocvec(&Name::from("héllo")).expect("postcard");
    assert_eq!(postcard::from_bytes::<Name>(&bytes).expect("parse"), "héllo");
}

#[test]
fn type_path_hash_matches_published_fnv1a_vectors() {
    // FNV-1a 64: the empty string hashes to the offset basis; "a" and "foobar" are the
    // vectors published with the algorithm.
    assert_eq!(TypePathHash::of("").get(), 0xcbf2_9ce4_8422_2325);
    assert_eq!(TypePathHash::of("a").get(), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(TypePathHash::of("foobar").get(), 0x8594_4171_f739_67e8);
}

#[test]
fn type_path_hash_is_const_and_distinguishes_paths() {
    const HASH: TypePathHash = TypePathHash::of("regia_scene::Transform");
    assert_eq!(HASH, TypePathHash::of("regia_scene::Transform"));
    assert_ne!(HASH, TypePathHash::of("regia_scene::transform"));
    assert_ne!(HASH, TypePathHash::of("regia_scene::Transform "));
}

// ---- privacy ---------------------------------------------------------------------------------

fn homes(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn scrub_without_a_match_borrows_the_input() {
    let out = scrub_with("nothing to see", &homes(&["/home/alice"]));
    assert!(matches!(out, Cow::Borrowed(_)));
    assert!(matches!(scrub_with("anything", &[]), Cow::Borrowed(_)));
}

#[test]
fn scrub_replaces_every_occurrence_of_the_home_prefix() {
    let out = scrub_with(
        "open /home/alice/a.txt then /home/alice/b/c",
        &homes(&["/home/alice"]),
    );
    assert_eq!(out, "open ~/a.txt then ~/b/c");
}

#[test]
fn scrub_leaves_a_longer_user_name_alone() {
    let text = "/home/alice2/x /home/alice-b/x /home/alice_b/x /home/aliceX";
    assert_eq!(scrub_with(text, &homes(&["/home/alice"])), text);
}

#[test]
fn scrub_treats_a_trailing_dot_as_punctuation_but_not_an_extension() {
    let homes = homes(&["/home/alice"]);
    assert_eq!(scrub_with("see /home/alice.", &homes), "see ~.");
    assert_eq!(scrub_with("see /home/alice. Next", &homes), "see ~. Next");
    assert_eq!(scrub_with("/home/alice.cfg", &homes), "/home/alice.cfg");
}

#[test]
fn scrub_handles_windows_style_paths_and_skips_empty_entries() {
    let homes = homes(&["", "C:\\Users\\bob"]);
    assert_eq!(scrub_with("C:\\Users\\bob\\file.txt", &homes), "~\\file.txt");
    assert_eq!(scrub_with("plain", &homes), "plain");
}

#[test]
fn scrub_at_the_very_end_of_the_text() {
    assert_eq!(scrub_with("/home/alice", &homes(&["/home/alice"])), "~");
    assert_eq!(scrub_with("(/home/alice)", &homes(&["/home/alice"])), "(~)");
}

// ---- Rng -----------------------------------------------------------------------------------

#[test]
fn rng_same_seed_same_sequence_and_streams_differ() {
    let mut a = Rng::new(99);
    let mut b = Rng::new(99);
    let mut other_seed = Rng::new(100);
    let mut other_stream = Rng::with_stream(99, 1);
    let first: Vec<u32> = (0..8).map(|_| a.next_u32()).collect();
    assert_eq!(first, (0..8).map(|_| b.next_u32()).collect::<Vec<_>>());
    assert_ne!(first, (0..8).map(|_| other_seed.next_u32()).collect::<Vec<_>>());
    assert_ne!(first, (0..8).map(|_| other_stream.next_u32()).collect::<Vec<_>>());
}

#[test]
fn rng_next_u64_is_high_word_first() {
    let mut a = Rng::new(5);
    let mut b = Rng::new(5);
    let hi = u64::from(a.next_u32());
    let lo = u64::from(a.next_u32());
    assert_eq!(b.next_u64(), (hi << 32) | lo);
}

#[test]
fn rng_floats_stay_in_the_half_open_unit_interval() {
    let mut rng = Rng::new(1);
    for _ in 0..2000 {
        let f = rng.f32();
        assert!((0.0..1.0).contains(&f), "{f}");
        let d = rng.f64();
        assert!((0.0..1.0).contains(&d), "{d}");
    }
}

#[test]
fn rng_chance_extremes_are_certain() {
    let mut rng = Rng::new(2);
    for _ in 0..500 {
        assert!(!rng.chance(0.0));
        assert!(rng.chance(1.0));
        assert!(rng.chance(2.0), "p above one is clamped, not an error");
        assert!(!rng.chance(-1.0));
    }
}

#[test]
fn rng_range_covers_inclusive_and_exclusive_bounds() {
    let mut rng = Rng::new(3);
    let mut seen = [false; 4];
    for _ in 0..400 {
        let v: usize = rng.range(0..=3);
        seen[v] = true;
    }
    assert_eq!(seen, [true; 4], "an inclusive range reaches both ends");
    for _ in 0..400 {
        let v: u32 = rng.range(10..13);
        assert!((10..13).contains(&v), "{v}");
        let f: f32 = rng.range(-2.0..2.0);
        assert!((-2.0..2.0).contains(&f), "{f}");
    }
}

#[test]
fn rng_empty_range_returns_its_start() {
    let mut rng = Rng::new(4);
    assert_eq!(rng.range(5u32..5), 5);
    assert_eq!(rng.range(7usize..=7), 7);
}

#[test]
fn rng_shuffle_is_a_deterministic_permutation() {
    let original: Vec<u32> = (0..32).collect();
    let mut one = original.clone();
    let mut two = original.clone();
    Rng::new(8).shuffle(&mut one);
    Rng::new(8).shuffle(&mut two);
    assert_eq!(one, two);
    assert_ne!(
        one, original,
        "32 elements practically never shuffle to the identity"
    );
    let mut sorted = one;
    sorted.sort_unstable();
    assert_eq!(sorted, original, "no element lost or duplicated");

    let mut empty: [u32; 0] = [];
    Rng::new(8).shuffle(&mut empty);
    let mut single = [1u32];
    Rng::new(8).shuffle(&mut single);
    assert_eq!(single, [1]);
}

#[test]
fn rng_choose_handles_empty_and_single_slices() {
    let mut rng = Rng::new(6);
    assert_eq!(rng.choose::<u32>(&[]), None);
    assert_eq!(rng.choose(&[42u32]), Some(&42));
    let items = [1u32, 2, 3];
    for _ in 0..50 {
        assert!(items.contains(rng.choose(&items).expect("non-empty")));
    }
}

#[test]
fn rng_fork_is_deterministic_and_independent_of_the_parent() {
    let mut p1 = Rng::new(11);
    let mut p2 = Rng::new(11);
    let mut f1 = p1.fork();
    let mut f2 = p2.fork();
    let child: Vec<u32> = (0..6).map(|_| f1.next_u32()).collect();
    assert_eq!(child, (0..6).map(|_| f2.next_u32()).collect::<Vec<_>>());
    let parent: Vec<u32> = (0..6).map(|_| p1.next_u32()).collect();
    assert_ne!(child, parent);
}

#[test]
fn rng_unit_vector_and_in_sphere_respect_their_radius() {
    let mut rng = Rng::new(12);
    for _ in 0..500 {
        let u = rng.unit_vector();
        assert!((u.length() - 1.0).abs() < 1e-4, "{u}");
        let p = rng.in_sphere();
        assert!(p.length() <= 1.0 + 1e-4, "{p}");
    }
}

#[test]
fn rng_from_entropy_differs_between_calls() {
    let mut a = Rng::from_entropy();
    let mut b = Rng::from_entropy();
    let sa: Vec<u64> = (0..4).map(|_| a.next_u64()).collect();
    let sb: Vec<u64> = (0..4).map(|_| b.next_u64()).collect();
    assert_ne!(sa, sb);
}

// ---- Curve ---------------------------------------------------------------------------------

#[test]
fn curve_accessors_on_an_empty_curve() {
    let curve = Curve::<f32>::default();
    assert!(curve.is_empty());
    assert_eq!(curve.start_time(), 0.0);
    assert_eq!(curve.end_time(), 0.0);
    assert_eq!(curve.duration(), 0.0);
    assert_eq!(curve.sample(3.0), None);
    assert_eq!(curve.sample_or(3.0, -1.0), -1.0);
}

#[test]
fn curve_new_sorts_and_reports_its_time_span() {
    let curve = Curve::linear([(2.0, 4.0f32), (0.5, 1.0), (1.0, 2.0)]);
    let times: Vec<f32> = curve.keys().iter().map(|k| k.time).collect();
    assert_eq!(times, [0.5, 1.0, 2.0]);
    assert_eq!(curve.start_time(), 0.5);
    assert_eq!(curve.end_time(), 2.0);
    assert_eq!(curve.duration(), 1.5);
}

#[test]
fn curve_sampling_clamps_outside_the_key_range() {
    let curve = Curve::linear([(1.0, 10.0f32), (3.0, 30.0)]);
    assert_eq!(curve.sample(-100.0), Some(10.0));
    assert_eq!(curve.sample(1.0), Some(10.0));
    assert!(close(curve.sample(2.0).expect("mid"), 20.0));
    assert_eq!(curve.sample(3.0), Some(30.0));
    assert_eq!(curve.sample(100.0), Some(30.0));
}

#[test]
fn curve_constant_is_the_same_everywhere() {
    let curve = Curve::constant(5.0f32);
    for t in [-10.0, 0.0, 0.5, 10.0] {
        assert_eq!(curve.sample(t), Some(5.0));
    }
}

#[test]
fn curve_step_holds_until_the_next_key() {
    let curve = Curve::new(vec![
        Keyframe::new(0.0, 1.0f32, Interp::Step, 0.0),
        Keyframe::new(1.0, 9.0, Interp::Step, 0.0),
    ]);
    assert_eq!(curve.sample(0.999), Some(1.0));
    assert_eq!(curve.sample(1.0), Some(9.0));
}

#[test]
fn curve_with_duplicate_times_jumps_without_dividing_by_zero() {
    let curve = Curve::linear([(0.0, 0.0f32), (1.0, 5.0), (1.0, 9.0), (2.0, 9.0)]);
    let at_jump = curve.sample(1.0).expect("sample");
    assert!(at_jump.is_finite());
    assert_eq!(at_jump, 9.0, "the later of two equal-time keys wins");
    assert!(close(curve.sample(0.5).expect("before"), 2.5));
    assert!(curve.sample(1.5).expect("after").is_finite());
}

#[test]
fn curve_insert_goes_after_existing_keys_with_the_same_time() {
    let mut curve = Curve::linear([(0.0, 0.0f32), (2.0, 2.0)]);
    curve.insert(Keyframe::new(1.0, 10.0, Interp::Linear, 0.0));
    curve.insert(Keyframe::new(1.0, 11.0, Interp::Linear, 0.0));
    let values: Vec<f32> = curve.keys().iter().map(|k| k.value).collect();
    assert_eq!(values, [0.0, 10.0, 11.0, 2.0]);
}

#[test]
fn curve_remove_out_of_range_is_none_and_leaves_keys_alone() {
    let mut curve = Curve::linear([(0.0, 1.0f32), (1.0, 2.0)]);
    assert!(curve.remove(2).is_none());
    assert_eq!(curve.keys().len(), 2);
    let removed = curve.remove(0).expect("first key");
    assert_eq!(removed.value, 1.0);
    assert_eq!(curve.keys().len(), 1);
    assert!(curve.remove(1).is_none());
}

#[test]
fn curve_hermite_with_zero_tangents_is_smoothstep() {
    let curve = Curve::new(vec![
        Keyframe::new(0.0, 0.0f32, Interp::CubicHermite, 0.0),
        Keyframe::new(1.0, 1.0, Interp::CubicHermite, 0.0),
    ]);
    assert!(close(curve.sample(0.5).expect("mid"), 0.5));
    // 3t^2 - 2t^3 at t = 0.25.
    assert!(close(curve.sample(0.25).expect("quarter"), 0.156_25));
    assert!(close(curve.sample(1.0).expect("end"), 1.0));
}

#[test]
fn curve_interpolates_vectors_and_rotations() {
    let curve = Curve::linear([(0.0, Vec3::ZERO), (2.0, Vec3::new(2.0, 4.0, 6.0))]);
    let mid = curve.sample(1.0).expect("mid");
    assert!(mid.abs_diff_eq(Vec3::new(1.0, 2.0, 3.0), 1e-5));

    let turn = Curve::linear([
        (0.0, Quat::IDENTITY),
        (1.0, Quat::from_rotation_y(std::f32::consts::FRAC_PI_2)),
    ]);
    let half = turn.sample(0.5).expect("mid");
    assert!((half.length() - 1.0).abs() < 1e-5, "slerp keeps unit length");
    assert!(half.abs_diff_eq(Quat::from_rotation_y(std::f32::consts::FRAC_PI_4), 1e-5));
}

#[test]
fn curve_deserialization_sorts_unordered_keys() {
    let key = |time: f32, value: f32| {
        format!(
            "{{\"time\":{time:?},\"value\":{value:?},\"interp\":\"Linear\",\"in_tangent\":0.0,\"out_tangent\":0.0}}"
        )
    };
    let json = format!(
        "{{\"keys\":[{},{},{}]}}",
        key(2.0, 20.0),
        key(0.0, 0.0),
        key(1.0, 10.0)
    );
    let curve: Curve<f32> = serde_json::from_str(&json).expect("curve");
    let times: Vec<f32> = curve.keys().iter().map(|k| k.time).collect();
    assert_eq!(times, [0.0, 1.0, 2.0]);
    assert!(close(curve.sample(1.5).expect("mid"), 15.0));
}

#[test]
fn curve_of_colours_interpolates_in_linear_space() {
    let curve = Curve::linear([(0.0, Color::BLACK), (1.0, Color::WHITE)]);
    let mid = curve.sample(0.5).expect("mid");
    assert!(
        close(mid.r, 0.5) && close(mid.g, 0.5) && close(mid.b, 0.5),
        "{mid:?}"
    );
}

// ---- scalar helpers and Isometry -------------------------------------------------------------

#[test]
fn inverse_lerp_and_remap_extrapolate_and_handle_reversed_ranges() {
    assert_eq!(inverse_lerp(0.0, 10.0, 15.0), 1.5);
    assert_eq!(inverse_lerp(10.0, 0.0, 2.5), 0.75);
    assert_eq!(remap(15.0, (0.0, 10.0), (0.0, 100.0)), 150.0);
    assert_eq!(remap(2.5, (10.0, 0.0), (0.0, 1.0)), 0.75);
    assert_eq!(remap(1.0, (3.0, 3.0), (5.0, 9.0)), 5.0);
}

#[test]
fn isometry_looking_at_rejects_degenerate_input() {
    let eye = Vec3::new(1.0, 2.0, 3.0);
    assert!(Isometry::looking_at(eye, eye, Vec3::Y).is_none(), "eye == target");
    assert!(
        Isometry::looking_at(eye, eye + Vec3::Y * 5.0, Vec3::Y).is_none(),
        "up parallel to the view direction"
    );
    assert!(
        Isometry::looking_at(eye, Vec3::ZERO, Vec3::ZERO).is_none(),
        "zero up"
    );
}

#[test]
fn isometry_looking_at_faces_negative_z_towards_the_target() {
    let eye = Vec3::new(1.0, 2.0, 3.0);
    let target = Vec3::new(4.0, 2.0, 3.0);
    let look = Isometry::looking_at(eye, target, Vec3::Y).expect("look");
    assert!(look.transform_point(Vec3::ZERO).abs_diff_eq(eye, 1e-5));
    assert!(look.transform_vector(-Vec3::Z).abs_diff_eq(Vec3::X, 1e-5));
    assert!(look.transform_vector(Vec3::Y).abs_diff_eq(Vec3::Y, 1e-5));
}

#[test]
fn isometry_inverse_round_trips_points_and_composes_to_identity() {
    let iso = Isometry::new(
        Quat::from_rotation_z(0.7) * Quat::from_rotation_x(-0.3),
        Vec3::new(3.0, -2.0, 5.0),
    );
    let p = Vec3::new(0.5, 1.5, -2.5);
    let back = iso.inverse().transform_point(iso.transform_point(p));
    assert!(back.abs_diff_eq(p, 1e-4), "{back}");
    let id = iso.to_mat4() * iso.inverse().to_mat4();
    assert!(id.abs_diff_eq(regia_core::glam::Mat4::IDENTITY, 1e-4));
}

#[test]
fn isometry_vectors_ignore_translation_and_interpolate_endpoints() {
    let a = Isometry::from_translation(Vec3::new(2.0, 0.0, 0.0));
    let b = Isometry::new(Quat::from_rotation_y(1.0), Vec3::new(0.0, 4.0, 0.0));
    assert_eq!(a.transform_vector(Vec3::Y), Vec3::Y);
    let start = a.interpolate(&b, 0.0);
    let end = a.interpolate(&b, 1.0);
    assert!(start.translation.abs_diff_eq(a.translation, 1e-6));
    assert!(end.translation.abs_diff_eq(b.translation, 1e-6));
    assert!(end.rotation.abs_diff_eq(b.rotation, 1e-5));
    let mid = a.interpolate(&b, 0.5);
    assert!(mid.translation.abs_diff_eq(Vec3::new(1.0, 2.0, 0.0), 1e-5));
}

/// The audit quat pair for the FMA-dependent-slerp finding (audit-g-core-determinism § 1):
/// identity → the 90° yaw whose components are the f32 nearest √2/2, `t = 0.5`. The dot
/// (~0.707) is below glam's lerp threshold, so the sine path runs — the path whose FMA use
/// split x86_64 (no `fma`) from aarch64 NEON. The deterministic slerp kernel must return these
/// exact bits on every target, and `Isometry::interpolate` and `Curve<Quat>` sampling must
/// both route through it (physics `time_of_impact` sweeps step `Isometry::interpolate`).
#[test]
fn isometry_and_curve_quat_interpolation_pins_rotation_bits() {
    let end = Quat::from_xyzw(0.0, f32::from_bits(0x3f35_04f3), 0.0, f32::from_bits(0x3f35_04f3));
    let a = Isometry::new(Quat::IDENTITY, Vec3::ZERO);
    let b = Isometry::new(end, Vec3::new(2.0, 0.0, 0.0));
    let mid = a.interpolate(&b, 0.5);
    assert_eq!(mid.rotation.x.to_bits(), 0x0000_0000);
    assert_eq!(mid.rotation.y.to_bits(), 0x3ec3_ef15);
    assert_eq!(mid.rotation.z.to_bits(), 0x0000_0000);
    assert_eq!(mid.rotation.w.to_bits(), 0x3f6c_835e);
    // Translation still lerps exactly.
    assert_eq!(mid.translation, Vec3::new(1.0, 0.0, 0.0));

    // The same pair through a sampled rotation curve takes the same deterministic path.
    let curve = Curve::linear([(0.0, Quat::IDENTITY), (1.0, end)]);
    let q = curve.sample(0.5).expect("two keys");
    assert_eq!(q.x.to_bits(), 0x0000_0000);
    assert_eq!(q.y.to_bits(), 0x3ec3_ef15);
    assert_eq!(q.z.to_bits(), 0x0000_0000);
    assert_eq!(q.w.to_bits(), 0x3f6c_835e);
}

#[test]
fn isometry_from_affine_discards_scale() {
    let affine = regia_core::glam::Affine3A::from_scale_rotation_translation(
        Vec3::new(2.0, 3.0, 4.0),
        Quat::from_rotation_y(0.5),
        Vec3::new(1.0, 1.0, 1.0),
    );
    let iso = Isometry::from_affine(&affine);
    assert!(iso.translation.abs_diff_eq(Vec3::ONE, 1e-5));
    assert!(iso.rotation.abs_diff_eq(Quat::from_rotation_y(0.5), 1e-5));
    assert!((iso.rotation.length() - 1.0).abs() < 1e-5);
}
