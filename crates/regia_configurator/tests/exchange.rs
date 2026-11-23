// SPDX-License-Identifier: MPL-2.0
//! Contract tests against the published exchange (`$JEWELRY_OUT/exchange/<name>/<major>/`, plan
//! `interfaces.md` § 0.6; ADR-2480, ADR-2535).
//!
//! Every file Regia reads is validated against the `schema.json` its owner published, not a copy:
//!
//! - **Exchange.** `REGIA_JW_EXCHANGE=<dir>` (default `<out>/exchange`). Each Regia-owned `schema.json`
//!   there must be byte-identical to `docs/formats/`. For the kit builder's interfaces (option graph,
//!   piece recipe) the `schema.json` must compile with this validator. Every fixture beside a schema
//!   must pass it, and every package fixture's folder must match its manifest.
//! - **Artifacts.** `REGIA_JW_OUT=<dir>` (default `$JEWELRY_OUT`): `kits/**/kit.json`,
//!   `materials/*/library.json`, `recipes/**/*.json`, `optiongraph/*/houses/*/products/*.json` and the
//!   packages under `packs/` (any `manifest.json` with a `schema`), each against its published schema.
//! - **Pilot.** `REGIA_JW_PILOT=<pilot.json>`: the chosen product of every slot has an option graph
//!   record and a recipe naming it.
//! - **`REGIA_JW_CONTRACT_REQUIRED=1`** (or `REGIA_JW_REQUIRE_PILOT=1`): any missing input fails the test instead of being reported
//!   (ADR-278: a contract run that silently checks nothing is worse than a red one).
//!
//! Without the variables the test only reports that nothing is configured; the committed fixtures
//! are covered by `tests/contract.rs`.

use std::path::{Path, PathBuf};

use regia_configurator::{Interface, Schema, check, check_bytes, checks};
use serde_json::Value;

fn required() -> bool {
    // `REGIA_JW_REQUIRE_PILOT` is spec 42's spelling of the same flag.
    ["REGIA_JW_CONTRACT_REQUIRED", "REGIA_JW_REQUIRE_PILOT"]
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|v| v == "1"))
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The outputs directory: `REGIA_JW_OUT`, else the plan's `$JEWELRY_OUT`.
fn out_dir() -> Option<PathBuf> {
    env_dir("REGIA_JW_OUT").or_else(|| env_dir("JEWELRY_OUT"))
}

/// The exchange directory: `REGIA_JW_EXCHANGE`, else `<out>/exchange`.
fn exchange_dir() -> Option<PathBuf> {
    env_dir("REGIA_JW_EXCHANGE").or_else(|| out_dir().map(|o| o.join("exchange")))
}

/// Every JSON file under `dir` that declares `interface` in its `schema` field. An owner may nest its
/// fixtures and keep other documents beside them (an index, a manifest, a package's inner files).
fn fixtures_of(dir: &Path, interface: Interface) -> Vec<PathBuf> {
    let mut all = Vec::new();
    find(
        dir,
        8,
        &|p: &Path| p.extension().is_some_and(|x| x == "json"),
        &mut all,
    );
    let id = interface.schema_id();
    all.retain(|p| {
        read_json(p).is_some_and(|doc| doc.get("schema").and_then(Value::as_str) == Some(id.as_str()))
    });
    all
}

/// Every `name` file under `dir`, at most `depth` levels down.
fn find(dir: &Path, depth: usize, pick: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            if depth > 0 {
                find(&p, depth - 1, pick, out);
            }
        } else if pick(&p) {
            out.push(p);
        }
    }
}

struct Run {
    problems: Vec<String>,
    checked: usize,
}

impl Run {
    fn missing(&mut self, what: String) {
        if required() {
            self.problems
                .push(format!("missing (REGIA_JW_CONTRACT_REQUIRED=1): {what}"));
        } else {
            eprintln!("contract: not present: {what}");
        }
    }

    fn validate(&mut self, path: &Path, interface: Interface, schema: Option<&Schema>) {
        self.checked += 1;
        let result = match std::fs::read(path) {
            Ok(bytes) => check_bytes(&bytes, Some(interface), schema).map(|_| ()),
            Err(e) => {
                self.problems.push(format!("{}: {e}", path.display()));
                return;
            }
        };
        if let Err(errors) = result {
            for e in errors.iter().take(10) {
                self.problems.push(format!("{}: {e}", path.display()));
            }
        }
    }
}

/// The schema to validate `interface`'s files with: always the file its owner published.
fn published_schema(run: &mut Run, dir: &Path, interface: Interface) -> Option<Schema> {
    let path = dir.join(interface.wire_name()).join("1").join("schema.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        run.missing(path.display().to_string());
        return None;
    };
    if let Some(ours) = interface.schema_text()
        && text != ours
    {
        run.problems.push(format!(
            "{}: differs from docs/formats/{}.schema.json (republish it)",
            path.display(),
            interface.format_name()
        ));
    }
    match Schema::parse(&text) {
        Ok(s) => Some(s),
        Err(e) => {
            run.problems
                .push(format!("{}: this validator cannot use it: {e}", path.display()));
            None
        }
    }
}

/// A package manifest's listed files, read from `folder`, and the JSON files inside against the schemas
/// published in `exchange`: the viewer reads those, so they are held to the contract too.
fn package_problems(run: &mut Run, exchange: &Path, manifest: &Path, folder: &Path) {
    let Some(doc) = read_json(manifest) else {
        return; // `validate` has reported the unreadable manifest.
    };
    let errors = checks::package_files(&doc, &mut |name| std::fs::read(folder.join(name)).ok());
    for e in errors.iter().take(10) {
        run.problems.push(format!("{}: {e}", manifest.display()));
    }
    let files = doc.get("files").and_then(Value::as_array);
    for file in files.map_or(&[][..], Vec::as_slice) {
        let interface = match file.get("kind").and_then(Value::as_str) {
            Some("recipe") => Interface::PieceRecipe,
            Some("options") => Interface::OptionGraph,
            Some("materials") => Interface::MaterialLibrary,
            Some("kit") => Interface::PartKit,
            _ => continue,
        };
        let Some(name) = file.get("name").and_then(Value::as_str) else {
            continue;
        };
        if let Some(schema) = published_schema(run, exchange, interface) {
            run.validate(&folder.join(name), interface, Some(&schema));
        }
    }
}

#[test]
fn published_exchange_matches_the_contracts() {
    let mut run = Run {
        problems: Vec::new(),
        checked: 0,
    };
    match exchange_dir() {
        None => run.missing("REGIA_JW_EXCHANGE or JEWELRY_OUT (the exchange directory)".into()),
        Some(dir) if !dir.is_dir() => run.missing(format!("exchange directory {}", dir.display())),
        Some(dir) => {
            for interface in Interface::ALL {
                let Some(schema) = published_schema(&mut run, &dir, interface) else {
                    continue;
                };
                let fixtures = dir.join(interface.wire_name()).join("1").join("fixtures");
                let files = fixtures_of(&fixtures, interface);
                if files.is_empty() {
                    run.missing(format!("fixtures in {}", fixtures.display()));
                }
                for f in files {
                    run.validate(&f, interface, Some(&schema));
                    if interface == Interface::ProductPackage {
                        package_problems(&mut run, &dir, &f, &f.with_extension(""));
                    }
                }
            }
            resolve_vectors(&mut run, &dir);
            companion_documents(&mut run, &dir);
        }
    }
    report(run);
}

/// Each row of the shared `resolve_vectors.jsonl` passes the row schema the option graph's owner
/// publishes in its `$defs`, so a harness compares against vectors of the agreed shape.
fn resolve_vectors(run: &mut Run, dir: &Path) {
    let root = dir.join(Interface::OptionGraph.wire_name()).join("1");
    let vectors = root.join("fixtures").join("resolve_vectors.jsonl");
    let Ok(text) = std::fs::read_to_string(&vectors) else {
        run.missing(vectors.display().to_string());
        return;
    };
    let Some(published) = read_json(&root.join("schema.json")) else {
        return; // `published_schema` has reported it.
    };
    let row = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": "#/$defs/resolve_vector",
        "$defs": published.get("$defs").cloned().unwrap_or(Value::Null),
    });
    let schema = match Schema::compile(row) {
        Ok(schema) => schema,
        Err(e) => {
            run.problems
                .push(format!("{}: no row schema: {e}", root.display()));
            return;
        }
    };
    for (i, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        run.checked += 1;
        let at = format!("{}:{}", vectors.display(), i + 1);
        match serde_json::from_str::<Value>(line) {
            Ok(doc) => {
                for v in schema.validate(&doc).iter().take(5) {
                    run.problems
                        .push(format!("{at}: {} {}", v.instance_path, v.message));
                }
            }
            Err(e) => run.problems.push(format!("{at}: {e}")),
        }
    }
}

/// An owner's schema can describe more than one document: its `x-documents` maps a `schema` id to a
/// pointer into the file. Every other JSON or JSONL document in the fixtures that declares one of those
/// ids passes the part the pointer names (the option graph's manifest, index, gaps and diff).
fn companion_documents(run: &mut Run, dir: &Path) {
    for interface in Interface::ALL {
        let root = dir.join(interface.wire_name()).join("1");
        let Some(published) = read_json(&root.join("schema.json")) else {
            continue; // `published_schema` has reported it.
        };
        let Some(documents) = published.get("x-documents").and_then(Value::as_object) else {
            continue;
        };
        let mut files = Vec::new();
        find(
            &root.join("fixtures"),
            8,
            &|p: &Path| p.extension().is_some_and(|x| x == "json" || x == "jsonl"),
            &mut files,
        );
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                run.problems.push(format!("{}: not readable", file.display()));
                continue;
            };
            let docs: Vec<Value> = if file.extension().is_some_and(|x| x == "jsonl") {
                text.lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect()
            } else {
                serde_json::from_str(&text).into_iter().collect()
            };
            for doc in docs {
                let id = doc.get("schema").and_then(Value::as_str).unwrap_or_default();
                let Some(pointer) = documents.get(id).and_then(Value::as_str) else {
                    continue;
                };
                if pointer == "#" {
                    continue; // The file's own interface, validated above.
                }
                let part = serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "$ref": pointer,
                    "$defs": published.get("$defs").cloned().unwrap_or(Value::Null),
                });
                run.checked += 1;
                match Schema::compile(part) {
                    Ok(schema) => {
                        for v in schema.validate(&doc).iter().take(5) {
                            run.problems.push(format!(
                                "{} ({id}): {} {}",
                                file.display(),
                                v.instance_path,
                                v.message
                            ));
                        }
                    }
                    Err(e) => run.problems.push(format!("{id} at {pointer}: {e}")),
                }
            }
        }
    }
}

#[test]
fn artifacts_regia_reads_match_their_schemas() {
    let mut run = Run {
        problems: Vec::new(),
        checked: 0,
    };
    let Some(out) = out_dir().filter(|d| d.is_dir()) else {
        run.missing("REGIA_JW_OUT or JEWELRY_OUT (the outputs directory)".into());
        return report(run);
    };
    let Some(exchange) = exchange_dir() else {
        return report(run);
    };
    let named = |n: &'static str| move |p: &Path| p.file_name().is_some_and(|f| f == n);
    let json = |p: &Path| p.extension().is_some_and(|x| x == "json");
    let mut kits = Vec::new();
    find(&out.join("kits"), 3, &named("kit.json"), &mut kits);
    let mut libs = Vec::new();
    find(&out.join("materials"), 2, &named("library.json"), &mut libs);
    let mut recipes = Vec::new();
    find(&out.join("recipes"), 2, &json, &mut recipes);
    let mut products = Vec::new();
    find(
        &out.join("optiongraph"),
        5,
        &|p: &Path| p.parent().is_some_and(|d| d.ends_with("products")) && json(p),
        &mut products,
    );
    let sets = [
        (Interface::PartKit, kits),
        (Interface::MaterialLibrary, libs),
        (Interface::PieceRecipe, recipes),
        (Interface::OptionGraph, products),
    ];
    for (interface, files) in sets {
        if files.is_empty() {
            run.missing(format!("{} files under {}", interface.wire_name(), out.display()));
            continue;
        }
        let Some(schema) = published_schema(&mut run, &exchange, interface) else {
            continue;
        };
        for f in files {
            run.validate(&f, interface, Some(&schema));
        }
    }
    // Packages come later than the kit builder's files, so none here is not an error yet.
    let mut manifests = Vec::new();
    find(&out.join("packs"), 3, &named("manifest.json"), &mut manifests);
    if !manifests.is_empty()
        && let Some(schema) = published_schema(&mut run, &exchange, Interface::ProductPackage)
    {
        for m in manifests {
            run.validate(&m, Interface::ProductPackage, Some(&schema));
            package_problems(&mut run, &exchange, &m, m.parent().unwrap_or(&out));
        }
    }
    report(run);
}

/// The chosen product of every slot in the plan's `pilot.json` has its option graph record and a
/// recipe that names it. The file is read at test time: nothing about the pilot enters the repo.
#[test]
fn pilot_products_have_their_inputs() {
    let mut run = Run {
        problems: Vec::new(),
        checked: 0,
    };
    let Some(pilot) = env_dir("REGIA_JW_PILOT") else {
        run.missing("REGIA_JW_PILOT (the plan's pilot.json)".into());
        return report(run);
    };
    let Some(doc) = read_json(&pilot) else {
        run.problems
            .push(format!("{}: not readable JSON", pilot.display()));
        return report(run);
    };
    let Some(out) = out_dir().filter(|d| d.is_dir()) else {
        run.missing("REGIA_JW_OUT or JEWELRY_OUT (the outputs directory)".into());
        return report(run);
    };
    let mut recipes = Vec::new();
    find(
        &out.join("recipes"),
        2,
        &|p: &Path| p.extension().is_some_and(|x| x == "json"),
        &mut recipes,
    );
    let recipes: Vec<Value> = recipes.iter().filter_map(|p| read_json(p)).collect();
    let mut products = Vec::new();
    find(
        &out.join("optiongraph"),
        5,
        &|p: &Path| p.parent().is_some_and(|d| d.ends_with("products")),
        &mut products,
    );
    let products: Vec<Value> = products.iter().filter_map(|p| read_json(p)).collect();
    let slots = doc.get("slots").and_then(Value::as_array);
    for slot in slots.map_or(&[][..], Vec::as_slice) {
        let Some(key) = slot.get("chosen").and_then(Value::as_str) else {
            run.problems.push(format!("pilot slot without `chosen`: {slot}"));
            continue;
        };
        run.checked += 1;
        if !products
            .iter()
            .any(|p| p.get("product_key").and_then(Value::as_str) == Some(key))
        {
            run.missing(format!("option graph record of {key}"));
        }
        let named = |r: &Value| {
            r.get("product_keys")
                .and_then(Value::as_array)
                .is_some_and(|keys| keys.iter().any(|k| k.as_str() == Some(key)))
        };
        if !recipes.iter().any(named) {
            run.missing(format!("recipe of {key}"));
        }
    }
    report(run);
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn report(run: Run) {
    eprintln!("contract: {} files checked", run.checked);
    assert!(
        run.problems.is_empty(),
        "contract problems:\n{}",
        run.problems.join("\n")
    );
}

/// The validator can read a schema of the shape the kit builder publishes
/// (2020-12, `$defs`, `oneOf`, patterns), so a contract run fails on the data, not the tool.
#[test]
fn validator_reads_a_consumer_style_schema() {
    let schema = Schema::compile(serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "required": ["schema", "house", "design_key", "view_range"],
        "properties": {
            "schema": {"const": "jewelry.piece_recipe/1"},
            "house": {"type": "string", "pattern": "^[a-z0-9-]+$"},
            "design_key": {"type": "string"},
            "view_range": {"$ref": "#/$defs/view_range"}
        },
        "$defs": {"view_range": {
            "type": "object",
            "properties": {"tilt_max_deg": {"type": "number", "minimum": 0, "maximum": 90},
                           "spin": {"enum": ["none", "rock", "about_front_axis", "free"]}},
            "required": ["tilt_max_deg", "spin"]
        }}
    }))
    .expect("consumer schema compiles");
    let ok: Value = serde_json::json!({"schema": "jewelry.piece_recipe/1", "house": "house-a",
        "design_key": "house-a/d0001", "view_range": {"tilt_max_deg": 30, "spin": "none"}});
    assert_eq!(
        check(&ok, Some(Interface::PieceRecipe), Some(&schema)),
        Ok(Interface::PieceRecipe)
    );
    let mut bad = ok.clone();
    bad["view_range"]["tilt_max_deg"] = serde_json::json!(120);
    let errors = check(&bad, Some(Interface::PieceRecipe), Some(&schema))
        .err()
        .unwrap_or_default();
    assert_eq!(
        errors.first().map(|e| (e.reason, e.path.as_str())),
        Some(("contract_out_of_range", "/view_range/tilt_max_deg"))
    );
}
