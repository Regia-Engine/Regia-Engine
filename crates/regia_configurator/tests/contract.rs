// SPDX-License-Identifier: MPL-2.0
//! The committed generic fixtures pass their schemas and checks, and each mutation is refused with
//! the reason (and F-code) its format page names (ADR-2480, ADR-2535).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_configurator::{ContractError, Interface, REASONS, check, check_bytes, checks};
use serde_json::{Value, json};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load(interface: Interface, file: &str) -> Value {
    let path = fixtures_dir().join(interface.format_name()).join(file);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn fixture_files(interface: Interface) -> Vec<PathBuf> {
    let dir = fixtures_dir().join(interface.format_name());
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files
}

/// The refusal reasons of `doc`, or an empty list when it passes.
fn reasons(doc: &Value, interface: Interface) -> Vec<(&'static str, &'static str, String)> {
    match check(doc, Some(interface), None) {
        Ok(_) => Vec::new(),
        Err(errors) => errors
            .into_iter()
            .map(|e: ContractError| (e.code, e.reason, e.path))
            .collect(),
    }
}

/// Applies `edit`, then asserts the document is refused with `reason` (at `path`, when given).
fn refused(
    interface: Interface,
    file: &str,
    edit: impl FnOnce(&mut Value),
    reason: &str,
    path: Option<&str>,
) {
    let mut doc = load(interface, file);
    edit(&mut doc);
    let got = reasons(&doc, interface);
    assert!(
        got.iter()
            .any(|(_, r, p)| *r == reason && path.is_none_or(|want| p == want)),
        "{file}: expected {reason} at {path:?}, got {got:?}"
    );
    let code = REASONS
        .iter()
        .find(|(r, _, _)| *r == reason)
        .map(|(_, c, _)| *c)
        .unwrap_or_else(|| panic!("{reason} is not in REASONS"));
    assert!(
        got.iter().any(|(c, r, _)| *r == reason && *c == code),
        "{reason} must answer {code}: {got:?}"
    );
}

#[test]
fn every_shipped_schema_compiles_and_names_its_interface() {
    for interface in Interface::OWNED {
        let text = interface.schema_text().expect("owned interfaces ship a schema");
        let doc: Value = serde_json::from_str(text).expect("schema is JSON");
        assert_eq!(doc["$schema"], "https://json-schema.org/draft/2020-12/schema");
        let schema = interface
            .schema()
            .unwrap_or_else(|| panic!("{interface:?} schema compiles"));
        let id = interface.schema_id();
        let pinned = schema.document().pointer("/properties/schema/const");
        assert_eq!(pinned.and_then(Value::as_str), Some(id.as_str()), "{interface:?}");
        assert!(!interface.schema_fingerprint().is_empty());
    }
    for interface in [Interface::OptionGraph, Interface::PieceRecipe] {
        assert!(interface.schema_text().is_none() && !interface.owned_by_regia());
    }
}

/// The crate compiles its own copies of the schemas (ADR-1485: a packaged or vendored crate has no
/// `docs/`), so the copies and the documented originals must stay byte-identical. A packaged copy has
/// no originals to compare with.
#[test]
fn shipped_schemas_are_byte_identical_to_the_documented_ones() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    if !manifest.join("../../Cargo.toml").is_file() {
        return;
    }
    let mut compared = 0;
    for interface in Interface::ALL {
        let Some(shipped) = interface.schema_text() else {
            continue;
        };
        let name = format!("{}.schema.json", interface.format_name());
        let documented = std::fs::read_to_string(manifest.join("../../docs/formats").join(&name))
            .unwrap_or_else(|e| panic!("docs/formats/{name}: {e}"));
        assert_eq!(
            shipped, documented,
            "copy crates/regia_configurator/assets/{name} over docs/formats/{name} (or the reverse)"
        );
        compared += 1;
    }
    assert_eq!(compared, 4, "the four schemas Regia owns");
}

#[test]
fn every_committed_fixture_passes() {
    let mut count = 0;
    for interface in Interface::OWNED {
        let files = fixture_files(interface);
        assert!(!files.is_empty(), "{interface:?} has no fixture");
        for path in files {
            let bytes = std::fs::read(&path).expect("read fixture");
            match check_bytes(&bytes, Some(interface), None) {
                Ok(i) => assert_eq!(i, interface),
                Err(e) => panic!("{}: {e:?}", path.display()),
            }
            count += 1;
        }
    }
    // A kit, a library, a build with a resolution and one the snapshot does not sell, a package.
    assert_eq!(count, 5, "fixture count");
}

#[test]
fn envelope_errors() {
    let build = load(Interface::Build, "build-house-a.json");
    let bytes = serde_json::to_vec(&build).expect("bytes");
    let err = |r: Result<Interface, Vec<ContractError>>| r.err().map(|e| e[0].reason);
    assert_eq!(
        err(check_bytes(b"{ not json", None, None)),
        Some("contract_not_json")
    );
    assert_eq!(
        err(check_bytes(&bytes, Some(Interface::PartKit), None)),
        Some("contract_wrong_interface")
    );
    assert_eq!(
        err(check(&json!({"a": 1}), None, None)),
        Some("contract_no_schema_field")
    );
    assert_eq!(
        err(check(&json!({"schema": "regia.render_out/2"}), None, None)),
        Some("contract_no_schema_field")
    );
    assert_eq!(
        err(check(&json!({"schema": "jewelry.nope/1"}), None, None)),
        Some("contract_unknown_interface")
    );
    assert_eq!(
        err(check(&json!({"schema": "jewelry.build/2"}), None, None)),
        Some("contract_unsupported_major")
    );
    let other = check(&json!({"schema": "jewelry.piece_recipe/1"}), None, None);
    assert_eq!(
        other.as_ref().err().map(|e| (e[0].code, e[0].reason)),
        Some(("F0003", "contract_no_schema"))
    );
    assert_eq!(check_bytes(&bytes, None, None), Ok(Interface::Build));
    // A file is bounded before it is parsed.
    let huge = vec![b' '; regia_configurator::MAX_FILE_BYTES + 1];
    assert_eq!(err(check_bytes(&huge, None, None)), Some("contract_too_large"));
    assert_eq!(
        err(check_bytes(b"not json", None, None)),
        Some("contract_not_json")
    );
}

/// The cancelled interfaces stay refused: their wire names are unknown, and a build carries no tenant.
#[test]
fn cancelled_interfaces_are_not_contracts() {
    for name in [
        "stream",
        "render_out",
        "session_record",
        "gem_transport_table",
        "content_manifest",
        "render_request",
        "shop_photo",
    ] {
        assert_eq!(Interface::from_wire_name(name), None, "{name}");
        let doc = json!({ "schema": format!("jewelry.{name}/1") });
        let got = reasons(&doc, Interface::Build);
        assert_eq!(got[0].1, "contract_unknown_interface", "{name}");
    }
    assert_eq!(Interface::ALL.len(), 6);
    assert_eq!(Interface::OWNED.len(), 4);
    let formats = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/formats");
    for name in [
        "stream",
        "render-out",
        "session-record",
        "gem-transport-table",
        "content-manifest",
    ] {
        assert!(
            !formats.join(format!("jewelry-{name}.schema.json")).exists(),
            "{name}"
        );
    }
    for interface in Interface::OWNED {
        assert!(
            formats
                .join(format!("{}.schema.json", interface.format_name()))
                .is_file(),
            "{interface:?}"
        );
        assert!(
            formats.join(format!("{}.md", interface.format_name())).is_file(),
            "{interface:?}"
        );
        assert_eq!(interface.format_version(), 1);
    }
}

#[test]
fn no_cost_and_no_client_data_anywhere() {
    // A build never carries cost, a price, a tenant or client data: the schema refuses every unknown field.
    for (pointer, field) in [
        ("", "cost"),
        ("/resolution", "cost"),
        ("", "price"),
        ("/resolution", "retail"),
        ("", "customer_email"),
        ("", "ip_address"),
        ("/camera", "customer"),
    ] {
        refused(
            Interface::Build,
            "build-house-a.json",
            |d| {
                d.pointer_mut(pointer)
                    .and_then(Value::as_object_mut)
                    .expect("object")
                    .insert(field.into(), json!(1));
            },
            "contract_unknown_field",
            Some(&format!("{pointer}/{field}")),
        );
    }
    refused(
        Interface::ProductPackage,
        "package-basic.json",
        |d| {
            d["customer_name"] = json!("x");
        },
        "contract_unknown_field",
        Some("/customer_name"),
    );
}

/// A resolution may carry the kit builder's reason objects and made-to-order fields as well as text.
#[test]
fn resolutions_take_the_kit_builders_structured_fields() {
    let hash = |d: &mut Value| {
        d["build_hash"] = json!(
            checks::build_hash(
                &d["camera"],
                &d["picks"],
                d["product_key"].as_str().expect("key"),
                d["recipe"]["hash"].as_str().expect("hash"),
                d["snapshot_id"].as_str().expect("snapshot"),
                &d["wear"],
            )
            .expect("hash")
        );
    };
    let mut no = load(Interface::Build, "build-not-sold.json");
    no["resolution"]["reasons"] = json!([{"code": "unknown_value", "group": "Material", "value": "Metal Z"}]);
    no["resolution"]["statement"] = json!(null);
    hash(&mut no);
    assert_eq!(reasons(&no, Interface::Build), Vec::new());
    no["resolution"]["reasons"] = json!([{"group": "Material"}]);
    assert!(
        !reasons(&no, Interface::Build).is_empty(),
        "a reason names its code"
    );

    let mut manual = load(Interface::Build, "build-house-a.json");
    manual["resolution"]["cart_mode"] = json!("manual_line");
    manual["resolution"]["cart_lines"] = json!([]);
    manual["resolution"]["made_to_order_line"] = json!({
        "statement": "End piece", "style_sku": "HA-0001", "variation_id": "1000", "metal_code": "1",
        "attrs": {"Thread": "6"},
    });
    hash(&mut manual);
    assert_eq!(reasons(&manual, Interface::Build), Vec::new());
    manual["resolution"]["made_to_order_line"]["cost"] = json!(1);
    assert!(
        reasons(&manual, Interface::Build)
            .iter()
            .any(|(_, r, _)| *r == "contract_unknown_field"),
        "no cost in a manual line"
    );
}

/// A tenant is optional, never a person, and a supplier tenant holds only its own house.
#[test]
fn tenant_ids_name_a_business_or_a_house() {
    for (interface, file) in [
        (Interface::Build, "build-house-a.json"),
        (Interface::ProductPackage, "package-basic.json"),
    ] {
        let mut doc = load(interface, file);
        assert!(
            doc.get("tenant_id").is_none(),
            "{file}: the fixture has no tenant"
        );
        doc["tenant_id"] = json!("shop:example-shop");
        assert_eq!(reasons(&doc, interface), Vec::new(), "{file}");
        doc["tenant_id"] = json!("house:house-a");
        assert_eq!(reasons(&doc, interface), Vec::new(), "{file}");
        refused(
            interface,
            file,
            |d| d["tenant_id"] = json!("house:house-b"),
            "contract_house_not_allowed",
            Some("/house"),
        );
        refused(
            interface,
            file,
            |d| d["tenant_id"] = json!("customer:someone"),
            "contract_bad_string",
            Some("/tenant_id"),
        );
    }
}

/// The build fields `interfaces.md` § 7 lists stay accepted, so a build written to that list passes
/// (with its hash taken over the camera it holds).
#[test]
fn a_build_written_to_the_plan_list_passes() {
    let mut doc = load(Interface::Build, "build-house-a.json");
    doc["build_id"] = json!("00000000-0000-4000-8000-000000000001");
    doc["studio_id"] = json!("studio-soft-1");
    doc["tier"] = json!("interactive");
    doc["output_gamut"] = json!("srgb");
    doc["front_still_ref"] = json!(null);
    doc["content_build_id"] = json!("unpackaged");
    for (key, value) in [
        ("rig_preset", json!("front")),
        ("tilt_xy_deg", json!([3.0, -4.0])),
        ("focal_length_mm", json!(100)),
        ("sensor_mm", json!(36)),
        ("focus_distance_mm", json!(60)),
        ("f_stop", json!(11)),
        ("dof", json!(false)),
    ] {
        doc["camera"][key] = value;
    }
    let as_object = doc.as_object_mut().expect("object");
    as_object.remove("sku");
    as_object.remove("house_variant_id");
    doc["build_hash"] = json!(
        checks::build_hash(
            &doc["camera"],
            &doc["picks"],
            doc["product_key"].as_str().expect("key"),
            doc["recipe"]["hash"].as_str().expect("hash"),
            doc["snapshot_id"].as_str().expect("snapshot"),
            &doc["wear"],
        )
        .expect("hash")
    );
    assert_eq!(reasons(&doc, Interface::Build), Vec::new());
}

#[test]
fn material_library_refusals() {
    let lib = "library-basic.json";
    let m = Interface::MaterialLibrary;
    refused(
        m,
        lib,
        |d| d["materials"][1]["material_id"] = json!("grey-metal-a"),
        "contract_duplicate_id",
        Some("/materials/1/material_id"),
    );
    refused(
        m,
        lib,
        |d| d["materials"][1]["catalogue_facets"] = json!(["Material=Metal A"]),
        "contract_duplicate_facet",
        Some("/materials/1/catalogue_facets/0"),
    );
    refused(
        m,
        lib,
        |d| d["materials"][2]["anodized"]["substrate_material_id"] = json!("missing"),
        "contract_unknown_reference",
        Some("/materials/2/anodized/substrate_material_id"),
    );
    refused(
        m,
        lib,
        |d| d["materials"][2]["anodized"]["substrate_material_id"] = json!("finish-polish"),
        "contract_wrong_kind",
        Some("/materials/2/anodized/substrate_material_id"),
    );
    refused(
        m,
        lib,
        |d| {
            let lut = d["materials"][2]["anodized"]["fresnel_lut"]["entries"]
                .as_array_mut()
                .expect("lut");
            lut.truncate(7);
        },
        "contract_bad_count",
        Some("/materials/2/anodized/fresnel_lut/entries"),
    );
    // A kind and a block that disagree.
    refused(
        m,
        lib,
        |d| d["materials"][0]["kind"] = json!("finish"),
        "contract_missing_field",
        None,
    );
    // A metal is a base color at least; an anodize a color or a table; a gem an index and a color.
    refused(
        m,
        lib,
        |d| {
            d["materials"][0]["conductor"]
                .as_object_mut()
                .expect("block")
                .remove("f0_rgb_linear");
        },
        "contract_missing_field",
        Some("/materials/0/conductor/f0_rgb_linear"),
    );
    refused(
        m,
        lib,
        |d| {
            d["materials"][3]["anodized"]
                .as_object_mut()
                .expect("block")
                .remove("color_rgb_linear");
        },
        "contract_missing_field",
        Some("/materials/3/anodized/fresnel_lut"),
    );
    refused(
        m,
        lib,
        |d| {
            d["materials"][4]["gem_dielectric"]
                .as_object_mut()
                .expect("block")
                .remove("attenuation_rgb_per_mm");
        },
        "contract_missing_field",
        Some("/materials/4/gem_dielectric/tint_rgb_linear"),
    );
    refused(
        m,
        lib,
        |d| d["materials"][4]["gem_dielectric"]["attenuation_rgb_per_mm"] = json!(null),
        "contract_missing_field",
        Some("/materials/4/gem_dielectric/tint_rgb_linear"),
    );
    refused(
        m,
        lib,
        |d| {
            d["materials"][7]["conductor"]["nk"] = json!({"wavelength_nm": [630.0, 532.0, 465.0], "n": [1.0, 1.0, 1.0], "k": [3.0, 3.0, 3.0, 3.0]});
        },
        "contract_bad_spectrum",
        None,
    );
    refused(
        m,
        lib,
        |d| d["materials"][4]["gem_dielectric"]["n_d"] = json!(0.9),
        "contract_out_of_range",
        Some("/materials/4/gem_dielectric/n_d"),
    );
    refused(
        m,
        lib,
        |d| d["materials"][4]["gem_dielectric"]["sparkle"] = json!(1.5),
        "contract_out_of_range",
        Some("/materials/4/gem_dielectric/sparkle"),
    );
    refused(
        m,
        lib,
        |d| d["materials"][9]["gem_dielectric"]["surface"] = json!("matte"),
        "contract_bad_value",
        Some("/materials/9/gem_dielectric/surface"),
    );
    refused(
        m,
        lib,
        |d| d["materials"][0]["calibration"]["method"] = json!("colorimeter"),
        "contract_bad_value",
        Some("/materials/0/calibration/method"),
    );
}

/// The physical fields of the plan's `interfaces.md` § 5 and § 6 stay accepted (the kit builder seeds
/// them), so a library or kit written to that list passes; the viewer just does not read them.
#[test]
fn physical_fields_are_accepted_and_optional() {
    let mut lib = load(Interface::MaterialLibrary, "library-basic.json");
    lib["materials"][0]["conductor"]["nk_dataset"] = json!("a-dataset");
    lib["materials"][0]["conductor"]["f82_tint_rgb"] = json!([0.8, 0.8, 0.8]);
    lib["materials"][0]["conductor"]["plating"] = json!("rhodium");
    lib["materials"][2]["anodized"]["oxide"] =
        json!({"model": "cauchy", "coefficients": [2.3, 0.05], "k": 0.0});
    lib["materials"][2]["anodized"]["thickness_nm"] = json!(120.0);
    lib["materials"][2]["anodized"]["fresnel_lut"]["inputs_hash"] =
        json!(format!("sha256:{}", "0".repeat(64)));
    lib["materials"][4]["gem_dielectric"]["ior_family"] = json!("ior-2-42");
    lib["materials"][4]["gem_dielectric"]["scatter"] = json!(0.0);
    lib["materials"][4]["gem_dielectric"]["inclusions"] = json!("none");
    assert_eq!(reasons(&lib, Interface::MaterialLibrary), Vec::new());
    let mut kit = load(Interface::PartKit, "kit-shared-basic.json");
    kit["gem_cuts"][0]["max_bounces_hint"] = json!(12);
    assert_eq!(reasons(&kit, Interface::PartKit), Vec::new());
}

#[test]
fn build_refusals() {
    let f = "build-house-a.json";
    let b = Interface::Build;
    refused(
        b,
        f,
        |d| d["camera"]["tilt_deg"] = json!(15),
        "contract_hash_mismatch",
        Some("/build_hash"),
    );
    refused(
        b,
        f,
        |d| d["picks"]["Material"] = json!("Metal B"),
        "contract_hash_mismatch",
        Some("/build_hash"),
    );
    refused(
        b,
        f,
        |d| d["product_key"] = json!("house-b/p0001"),
        "contract_key_mismatch",
        Some("/product_key"),
    );
    refused(
        b,
        f,
        |d| d["recipe"]["design_key"] = json!("house-a/d0002"),
        "contract_key_mismatch",
        Some("/recipe/design_key"),
    );
    refused(
        b,
        f,
        |d| d["sku"] = json!("ANOTHER-SKU"),
        "contract_inconsistent_resolution",
        Some("/sku"),
    );
    refused(
        b,
        f,
        |d| d["house_variant_id"] = json!("1000:2"),
        "contract_inconsistent_resolution",
        Some("/house_variant_id"),
    );
    refused(
        b,
        f,
        |d| d["house_variant_id"] = json!(null),
        "contract_inconsistent_resolution",
        Some("/house_variant_id"),
    );
    refused(
        b,
        f,
        |d| d["resolution"]["sku"] = json!(null),
        "contract_inconsistent_resolution",
        Some("/resolution/sku"),
    );
    refused(
        b,
        f,
        |d| d["resolution"]["cart_lines"] = json!([]),
        "contract_inconsistent_resolution",
        Some("/resolution/cart_lines"),
    );
    refused(
        b,
        f,
        |d| d["resolution"]["orderable"] = json!(false),
        "contract_inconsistent_resolution",
        Some("/resolution/reasons"),
    );
    refused(
        b,
        f,
        |d| d["camera"]["tilt_deg"] = json!(91),
        "contract_out_of_range",
        Some("/camera/tilt_deg"),
    );
    refused(
        b,
        f,
        |d| d["camera"]["rig_preset"] = json!("orbit"),
        "contract_bad_value",
        Some("/camera/rig_preset"),
    );
    refused(
        b,
        f,
        |d| d["snapshot_id"] = json!("latest"),
        "contract_bad_string",
        Some("/snapshot_id"),
    );
    // Picks: a multi group takes a list of labels, nothing else.
    refused(
        b,
        f,
        |d| d["picks"]["Extras"] = json!([1]),
        "contract_wrong_type",
        None,
    );
    // A build the snapshot does not sell names why and carries no SKU.
    let not_sold = "build-not-sold.json";
    refused(
        b,
        not_sold,
        |d| d["resolution"]["reasons"] = json!([]),
        "contract_inconsistent_resolution",
        Some("/resolution/reasons"),
    );
    refused(
        b,
        not_sold,
        |d| d["sku"] = json!("HA-0001-A"),
        "contract_inconsistent_resolution",
        Some("/sku"),
    );
}

#[test]
fn product_package_refusals() {
    let f = "package-basic.json";
    let p = Interface::ProductPackage;
    // Refreshes `package_hash` so a test reaches the check it is about.
    let rehash = |d: &mut Value| {
        d["package_hash"] = json!(checks::package_hash(&d["files"]).expect("hash"));
    };
    let resum = |d: &mut Value| {
        let total: u64 = d["files"]
            .as_array()
            .expect("files")
            .iter()
            .filter_map(|x| x["size"].as_u64())
            .sum();
        d["sizes"]["total"] = json!(total);
    };
    refused(
        p,
        f,
        |d| d["sizes"]["total"] = json!(1),
        "contract_package_size_mismatch",
        Some("/sizes/total"),
    );
    refused(
        p,
        f,
        |d| {
            d["files"][0]["size"] = json!(65);
            resum(d);
        },
        "contract_hash_mismatch",
        Some("/package_hash"),
    );
    refused(
        p,
        f,
        |d| {
            d["files"][1]["name"] = d["files"][0]["name"].clone();
            rehash(d);
        },
        "contract_duplicate_id",
        Some("/files/1/name"),
    );
    for kind in ["recipe", "options", "materials", "mesh"] {
        refused(
            p,
            f,
            |d| {
                d["files"]
                    .as_array_mut()
                    .expect("files")
                    .retain(|x| x["kind"] != kind);
                resum(d);
                rehash(d);
            },
            "contract_package_incomplete",
            Some("/files"),
        );
    }
    refused(
        p,
        f,
        |d| {
            let files = d["files"].as_array_mut().expect("files");
            let mut again = files[2].clone();
            again["name"] = json!("recipe2.json");
            again["size"] = json!(0);
            files.push(again);
            rehash(d);
        },
        "contract_package_incomplete",
        Some("/files"),
    );
    refused(
        p,
        f,
        |d| d["product_key"] = json!("house-b/p0001"),
        "contract_key_mismatch",
        Some("/product_key"),
    );
    refused(
        p,
        f,
        |d| d["recipe"]["design_key"] = json!("house-a/other"),
        "contract_key_mismatch",
        Some("/recipe/design_key"),
    );
    for bad in ["../outside.bin", "sub/mesh.rvm", ".hidden", ""] {
        refused(
            p,
            f,
            |d| d["files"][0]["name"] = json!(bad),
            "contract_bad_string",
            Some("/files/0/name"),
        );
    }
    refused(
        p,
        f,
        |d| d["files"][0]["kind"] = json!("video"),
        "contract_bad_value",
        Some("/files/0/kind"),
    );
    refused(
        p,
        f,
        |d| d["format_version"] = json!(0),
        "contract_out_of_range",
        Some("/format_version"),
    );
    refused(
        p,
        f,
        |d| d["files"][0]["blake3"] = json!("sha256:00"),
        "contract_bad_string",
        Some("/files/0/blake3"),
    );
}

/// `checks::package_files` over the committed package folder, then with each file missing, resized,
/// altered, and with a cost field smuggled into a JSON file.
#[test]
fn package_files_match_the_manifest() {
    let dir = fixtures_dir().join("jewelry-product-package/package-basic");
    let manifest = load(Interface::ProductPackage, "package-basic.json");
    let mut store: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for row in manifest["files"].as_array().expect("files") {
        let name = row["name"].as_str().expect("name");
        store.insert(
            name.to_owned(),
            std::fs::read(dir.join(name)).expect("package file"),
        );
    }
    let run = |store: &BTreeMap<String, Vec<u8>>| {
        checks::package_files(&manifest, &mut |p| store.get(p).cloned())
            .into_iter()
            .map(|e| (e.code, e.reason, e.path))
            .collect::<Vec<_>>()
    };
    assert_eq!(run(&store), Vec::new());

    let mut absent = store.clone();
    absent.remove("mesh.rvm");
    assert_eq!(
        run(&absent),
        vec![(
            "F0003",
            "contract_package_file_missing",
            "/files/0/name".to_owned()
        )]
    );

    let mut resized = store.clone();
    resized.get_mut("mesh.rvm").expect("mesh").push(0);
    let got = run(&resized);
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(
        got.iter()
            .all(|(c, r, _)| *c == "F0002" && *r == "contract_package_file_mismatch")
    );

    let mut altered = store.clone();
    altered.get_mut("mesh.rvm").expect("mesh")[0] ^= 1;
    assert_eq!(
        run(&altered),
        vec![(
            "F0002",
            "contract_package_file_mismatch",
            "/files/0/blake3".to_owned()
        )]
    );

    let mut options: Value =
        serde_json::from_slice(&store["options.json"]).expect("the options file is JSON");
    options["variants"] = json!([{ "variant_key": "1000:1", "Cost": 48.5 }]);
    let mut costed = store.clone();
    costed.insert(
        "options.json".to_owned(),
        serde_json::to_vec(&options).expect("bytes"),
    );
    assert!(
        run(&costed)
            .iter()
            .any(|(_, r, p)| *r == "contract_package_has_cost" && p == "/files/3"),
        "{:?}",
        run(&costed)
    );
    assert_eq!(
        checks::find_cost_field(&options).as_deref(),
        Some("/variants/0/Cost")
    );
    for name in ["cost", "price", "price_delta", "markup", "retail", "dealer_cost"] {
        let nested = json!({ "a": [{ "b": { (name): 1 } }] });
        assert_eq!(
            checks::find_cost_field(&nested),
            Some(format!("/a/0/b/{name}")),
            "{name}"
        );
    }
    assert_eq!(
        checks::find_cost_field(&json!({ "statement": "cost", "label": ["price"] })),
        None,
        "only field names count"
    );
}

#[test]
fn package_hash_is_the_blake3_of_the_canonical_file_list() {
    let doc = load(Interface::ProductPackage, "package-basic.json");
    // The fixture's hash was computed by the Python generator's own RFC 8785 code: both agree.
    assert_eq!(
        checks::package_hash(&doc["files"]).as_deref(),
        doc["package_hash"].as_str()
    );
}

#[test]
fn part_kit_refusals() {
    let kit = "kit-shared-basic.json";
    let k = Interface::PartKit;
    refused(
        k,
        kit,
        |d| d["parts"][1]["part_id"] = d["parts"][0]["part_id"].clone(),
        "contract_duplicate_id",
        Some("/parts/1/part_id"),
    );
    refused(
        k,
        kit,
        |d| d["parts"][0]["kind"] = json!("ring"),
        "contract_bad_value",
        Some("/parts/0/kind"),
    );
    refused(
        k,
        kit,
        |d| d["parts"][0]["glb"] = json!("../outside.glb"),
        "contract_bad_string",
        Some("/parts/0/glb"),
    );
    refused(
        k,
        kit,
        |d| d["parts"][0]["glb"] = json!("other.glb"),
        "contract_unknown_reference",
        Some("/parts/0/glb"),
    );
    refused(
        k,
        kit,
        |d| {
            d["parts"][0]
                .as_object_mut()
                .expect("part")
                .remove("edge_radius_min_mm");
        },
        "contract_missing_field",
        Some("/parts/0/edge_radius_min_mm"),
    );
    refused(
        k,
        kit,
        |d| d["parts"][1]["params"][0]["min"] = json!(9.0),
        "contract_bad_range",
        Some("/parts/1/params/0/min"),
    );
    refused(
        k,
        kit,
        |d| d["parts"][1]["params"][0]["default"] = json!(7.0),
        "contract_bad_range",
        Some("/parts/1/params/0/default"),
    );
    refused(
        k,
        kit,
        |d| d["parts"][1]["params"][0]["step"] = json!(0),
        "contract_out_of_range",
        Some("/parts/1/params/0/step"),
    );
    refused(
        k,
        kit,
        |d| d["parts"][0]["sockets"][0]["transform"]["rotation_quat"] = json!([0, 0, 0.5, 0.5]),
        "contract_bad_transform",
        Some("/parts/0/sockets/0/transform/rotation_quat"),
    );
    refused(
        k,
        kit,
        |d| d["parts"][0]["lods"][1]["screen_size"] = json!(1.0),
        "contract_bad_lods",
        Some("/parts/0/lods/1"),
    );
    refused(
        k,
        kit,
        |d| d["parts"][0]["front_axis"] = json!([0, 0, 0.5]),
        "contract_bad_transform",
        Some("/parts/0/front_axis"),
    );
    refused(
        k,
        kit,
        |d| d["units"] = json!("cm"),
        "contract_bad_value",
        Some("/units"),
    );
}

#[test]
fn gem_cut_refusals() {
    let kit = "kit-shared-basic.json";
    let k = Interface::PartKit;
    // A facet normal off unit length.
    refused(
        k,
        kit,
        |d| d["gem_cuts"][0]["facets"][0]["normal"] = json!([0, 0, 0.9]),
        "contract_bad_normal",
        Some("/gem_cuts/0/facets/0/normal"),
    );
    // Drop the table: the solid stays closed (the crown planes meet in a point), but lose every
    // crown facet and the top is open.
    refused(
        k,
        kit,
        |d| {
            let facets = d["gem_cuts"][0]["facets"].as_array_mut().expect("facets");
            facets.retain(|f| f["kind"] != "crown" && f["kind"] != "table");
        },
        "contract_cut_unbounded",
        Some("/gem_cuts/0/facets"),
    );
    // A second table plane far above the stone touches nothing.
    refused(
        k,
        kit,
        |d| {
            let facets = d["gem_cuts"][1]["facets"].as_array_mut().expect("facets");
            facets.push(json!({"kind": "table", "normal": [0.0, 0.0, 1.0], "offset_mm": 3.0}));
        },
        "contract_cut_redundant_facet",
        None,
    );
    // A second copy of the table lies on a face the cut already has. Clipping that plane used to
    // keep the old face and add another, so the duplicate still had area and the cut was accepted.
    refused(
        k,
        kit,
        |d| {
            let facets = d["gem_cuts"][0]["facets"].as_array_mut().expect("facets");
            let table = facets[0].clone();
            facets.push(table);
        },
        "contract_cut_redundant_facet",
        None,
    );
    // Planes that exclude each other leave nothing.
    refused(
        k,
        kit,
        |d| {
            let facets = d["gem_cuts"][0]["facets"].as_array_mut().expect("facets");
            facets.push(json!({"kind": "other", "normal": [0.0, 0.0, -1.0], "offset_mm": -5.0}));
        },
        "contract_cut_empty",
        Some("/gem_cuts/0/facets"),
    );
    refused(
        k,
        kit,
        |d| d["gem_cuts"][1]["size_mm"] = json!([4.5, 2.0]),
        "contract_cut_size_mismatch",
        Some("/gem_cuts/1/size_mm"),
    );
    refused(
        k,
        kit,
        |d| d["gem_cuts"][0]["proportions"]["crown_height_pct"] = json!(20.0),
        "contract_cut_proportions_mismatch",
        Some("/gem_cuts/0/proportions/crown_height_pct"),
    );
    refused(
        k,
        kit,
        |d| d["gem_cuts"][0]["convex"] = json!(false),
        "contract_missing_field",
        Some("/gem_cuts/0/mesh_ref"),
    );
    refused(
        k,
        kit,
        |d| d["gem_cuts"][0]["facets"] = json!([]),
        "contract_cut_no_facets",
        Some("/gem_cuts/0/facets"),
    );
    // A concave cut with a mesh listed in `files` is accepted without a plane check.
    let mut doc = load(k, kit);
    doc["gem_cuts"][0]["convex"] = json!(false);
    doc["gem_cuts"][0]["mesh_ref"] = json!({"glb": "shared_parts.glb", "node": "cut_mesh"});
    assert_eq!(reasons(&doc, k), Vec::new());
}

/// The generated round brilliant is Tolkowsky's: crown height about 16.2 % and pavilion depth
/// about 43.1 % of the diameter, and the cut check measures them from the planes themselves.
#[test]
fn round_brilliant_fixture_measures_tolkowsky() {
    let doc = load(Interface::PartKit, "kit-shared-basic.json");
    let cut = &doc["gem_cuts"][0];
    assert_eq!(cut["cut_type"], "round_brilliant");
    let p = &cut["proportions"];
    assert!((p["crown_height_pct"].as_f64().unwrap_or(0.0) - 16.2).abs() < 0.1);
    assert!((p["pavilion_depth_pct"].as_f64().unwrap_or(0.0) - 43.1).abs() < 0.1);
    // Nudging the pavilion depth by 1.5 points is caught: the check measured the planes.
    refused(
        Interface::PartKit,
        "kit-shared-basic.json",
        |d| d["gem_cuts"][0]["proportions"]["pavilion_depth_pct"] = json!(44.6),
        "contract_cut_proportions_mismatch",
        Some("/gem_cuts/0/proportions/pavilion_depth_pct"),
    );
}

#[test]
fn build_hash_is_jcs_sha256_of_the_covered_fields() {
    let doc = load(Interface::Build, "build-house-a.json");
    let want = checks::build_hash(
        &doc["camera"],
        &doc["picks"],
        doc["product_key"].as_str().unwrap_or_default(),
        doc["recipe"]["hash"].as_str().unwrap_or_default(),
        doc["snapshot_id"].as_str().unwrap_or_default(),
        &doc["wear"],
    );
    // The fixture's hash was computed by the Python generator's own RFC 8785 code: both agree.
    assert_eq!(want.as_deref(), doc["build_hash"].as_str());
    // Integer and float spellings of one number hash the same (RFC 8785 numbers are doubles).
    let mut float = doc.clone();
    float["camera"]["pan_mm"] = json!([0.0, 0.0]);
    float["camera"]["spin_deg"] = json!(0.0);
    assert_eq!(reasons(&float, Interface::Build), Vec::new());
}

#[test]
fn every_reason_is_documented_on_a_format_page() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/formats");
    let page = std::fs::read_to_string(root.join("jewelry-contracts.md"))
        .expect("docs/formats/jewelry-contracts.md");
    for (reason, code, _) in REASONS {
        assert!(
            page.contains(&format!("`{reason}`")),
            "{reason} missing from jewelry-contracts.md"
        );
        assert!(code.starts_with('F') && code.len() == 5);
        assert!(
            reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{reason}"
        );
    }
}

#[test]
fn upgrade_project_probe_recognises_only_its_interface() {
    let build = std::fs::read(fixtures_dir().join("jewelry-build/build-house-a.json")).expect("read");
    assert_eq!(Interface::Build.probe_file_version(&build), Ok(Some(1)));
    assert_eq!(Interface::PartKit.probe_file_version(&build), Ok(None));
    assert_eq!(
        Interface::Build.probe_file_version(b"{\"schema\": \"jewelry.build/2\"}"),
        Ok(Some(2))
    );
    assert_eq!(Interface::Build.probe_file_version(b"[1, 2]"), Ok(None));
    assert_eq!(Interface::Build.probe_file_version(b"not json"), Ok(None));
}

/// The committed `docs/formats/format-hashes.ron` holds each schema's check-formats hash: BLAKE3 of
/// the RFC 8785 text of the schema (`Interface::schema_fingerprint`), at its format version.
#[test]
fn committed_format_hashes_match_the_schemas() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/formats/format-hashes.ron");
    let table = std::fs::read_to_string(root).expect("format-hashes.ron");
    for interface in Interface::OWNED {
        let hash = blake3::hash(interface.schema_fingerprint().as_bytes())
            .to_hex()
            .to_string();
        let entry = format!(
            "\"{}\": (\n            version: {},\n            hash: \"{hash}\",\n        ),",
            interface.format_name(),
            interface.format_version()
        );
        assert!(
            table.contains(&entry),
            "{} needs `cargo xtask check-formats --update` (hash {hash})",
            interface.format_name()
        );
    }
}

/// Two file `size` values of `u64::MAX` overflow a debug `sum()` and used to panic the package
/// check. The schema's per-file maximum keeps a valid package off this path.
#[test]
fn contract_checks_do_not_panic_when_file_sizes_overflow() {
    let doc = json!({
        "schema": "jewelry.product_package/1",
        "house": "house-a",
        "product_key": "house-a/p1",
        "design_key": "house-a/p1",
        "files": [
            {"name": "recipe.json", "kind": "recipe", "size": u64::MAX},
            {"name": "options.json", "kind": "options", "size": u64::MAX},
            {"name": "materials.json", "kind": "materials", "size": 0},
            {"name": "mesh.rvm", "kind": "mesh", "size": 0}
        ],
        "sizes": {"total": 0}
    });
    let got: Vec<(String, String)> = checks::run(Interface::ProductPackage, &doc)
        .into_iter()
        .map(|e| (e.reason.to_owned(), e.path))
        .collect();
    assert!(
        got.iter()
            .any(|(r, p)| r == "contract_package_size_mismatch" && p == "/sizes/total"),
        "{got:?}"
    );
}

/// A convex cut whose facet has no plane used to return before the closure proof, so the contract
/// checks (the path a build without the schema validator still runs) accepted it.
#[test]
fn contract_checks_refuse_a_convex_cut_whose_facet_has_no_plane() {
    let mut doc = load(Interface::PartKit, "kit-shared-basic.json");
    doc["gem_cuts"][0]["facets"][0]
        .as_object_mut()
        .expect("facet")
        .remove("offset_mm");
    let got: Vec<(String, String)> = checks::run(Interface::PartKit, &doc)
        .into_iter()
        .map(|e| (e.reason.to_owned(), e.path))
        .collect();
    assert!(
        got.iter()
            .any(|(r, p)| r == "contract_missing_field" && p.contains("facets/0")),
        "{got:?}"
    );
}

/// Audit fix (sprint-fix-audit-jw, finding 7): with the validator, the schema's `maxItems` refuses a cut
/// with more than 512 facets before the contract checks run. The contract checks also run alone (a build
/// without the `validate` feature, or a kit builder schema without the limit), and there they clipped
/// every plane of the cut, work that grows with the square of the count, bounded only by the kit's
/// 32,768. They now refuse such a cut by count and never clip it: the repeated planes below would each be
/// a `contract_cut_redundant_facet` if they did.
#[test]
fn the_contract_checks_refuse_a_cut_with_too_many_facets_and_never_clip_it() {
    let mut doc = load(Interface::PartKit, "kit-shared-basic.json");
    let facets = doc["gem_cuts"][0]["facets"].as_array_mut().expect("facets");
    let first = facets[0].clone();
    while facets.len() <= regia_configurator::checks::MAX_CUT_FACETS {
        facets.push(first.clone());
    }
    let got: Vec<(&str, String)> = checks::run(Interface::PartKit, &doc)
        .into_iter()
        .map(|e| (e.reason, e.path))
        .collect();
    assert!(
        got.iter()
            .any(|(r, p)| *r == "contract_bad_count" && p == "/gem_cuts/0/facets"),
        "{got:?}"
    );
    assert!(
        !got.iter()
            .any(|(r, p)| r.starts_with("contract_cut_") && p.starts_with("/gem_cuts/0/")),
        "the cut over the limit was clipped: {got:?}"
    );
    // With the validator the schema refuses it by count too.
    let all = reasons(&doc, Interface::PartKit);
    assert!(
        all.iter()
            .any(|(_, r, p)| *r == "contract_bad_count" && p == "/gem_cuts/0/facets"),
        "{all:?}"
    );
}
