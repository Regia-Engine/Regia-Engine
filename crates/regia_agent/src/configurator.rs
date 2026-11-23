// SPDX-License-Identifier: MPL-2.0
//! `configurator.contract_check` and `configurator.contract_schema`: the product viewer
//! contracts (`jewelry.*` JSON, ADR-2480, ADR-2535, `book/agents/configurator-contracts.md`).
//!
//! Both write nothing and read no world state, except that `package_dir` reads a package folder in
//! the project. An invalid document is a result (`ok: false` with the refusals), not a call error;
//! only malformed parameters answer `F0002`, and a `package_dir` that does not exist `F0003`.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use regia_app::prelude::World;
use regia_configurator::{Interface, Schema};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// The interface a `interface` parameter names (`part_kit`, `jewelry.part_kit/1` or
/// `jewelry-part-kit`), `Ok(None)` when absent.
fn interface_param(params: &Value) -> Result<Option<Interface>, RpcError> {
    let Some(raw) = params.get("interface") else {
        return Ok(None);
    };
    let Some(name) = raw.as_str() else {
        return Err(RpcError::invalid_params("`interface` is a string"));
    };
    let found = Interface::from_wire_name(name)
        .or_else(|| {
            regia_configurator::parse_schema_id(name).and_then(|(wire, _)| Interface::from_wire_name(wire))
        })
        .or_else(|| Interface::ALL.into_iter().find(|i| i.format_name() == name));
    found.map(Some).ok_or_else(|| {
        let known: Vec<&str> = Interface::ALL.iter().map(|i| i.wire_name()).collect();
        RpcError::invalid_params(format!("unknown interface `{name}`; known: {}", known.join(", ")))
    })
}

/// The folder a `package_dir` parameter names, canonical. The request-path rule (ADR-1816) refuses
/// a hostile string before the file system sees it; what the file system then says is answered by
/// kind, never in the OS's words.
fn package_dir_param(world: &World, dir: &str) -> Result<PathBuf, RpcError> {
    let path = crate::req_path::project_path(world, "package_dir", dir)?;
    let resolved = path.canonicalize().map_err(|e| match e.kind() {
        ErrorKind::NotFound | ErrorKind::NotADirectory => absent_package_dir(dir),
        // Past the whole-path limit, glibc's `realpath` walks and stops at the first absent folder
        // (NotFound) while macOS refuses the length before looking (InvalidFilename). Answer the
        // same on every host: an absent folder on the way is "does not exist" (ADR-5550).
        ErrorKind::InvalidFilename if has_absent_folder(&path) => absent_package_dir(dir),
        kind => RpcError::invalid_params(format!("`package_dir` cannot be read: {kind}")),
    })?;
    if !resolved.is_dir() {
        return Err(RpcError::invalid_params("`package_dir` is not a folder"));
    }
    Ok(resolved)
}

/// The `F0003` answer for a `package_dir` that does not exist.
fn absent_package_dir(dir: &str) -> RpcError {
    RpcError::not_found(
        format!("`package_dir` `{}` does not exist", dir.escape_debug()),
        "check the path with assets.list or ls",
    )
}

/// True when a folder on the way to `path` is absent, checked one component at a time (each prefix
/// is short until the walk reaches it, unlike the whole path). A component the file system refuses
/// (a segment over the name limit) or cannot read stops the walk with false.
fn has_absent_folder(path: &Path) -> bool {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        match prefix.symlink_metadata() {
            Ok(_) => {}
            Err(e) => return matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory),
        }
    }
    false
}

/// The files of a package folder against its manifest. A listed name that breaks the request-path
/// rule, a symlink that leads out and anything but a regular file (a FIFO would block the read) all
/// read as absent.
fn package_dir_errors(dir: &Path, manifest: &Value) -> Vec<regia_configurator::ContractError> {
    let inside = |path: &str| {
        regia_core::project_path::resolve_read(dir, path)
            .ok()
            .filter(|file| file.is_file())
    };
    regia_configurator::checks::package_files(manifest, &mut |path| std::fs::read(inside(path)?).ok())
}

/// `configurator.contract_check { document, interface?, schema?, package_dir? }`.
pub fn contract_check(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(document) = params.get("document") else {
        return Err(RpcError::invalid_params(
            "`document` (the JSON file's content) is required",
        ));
    };
    let expected = interface_param(params)?;
    // A JSON Schema document is an object; a wrong-typed value is refused, never compiled
    // (ADR-2622).
    let published = match crate::params::opt_object(params, "schema")? {
        None => None,
        Some(doc) => Some(
            Schema::compile(Value::Object(doc.clone()))
                .map_err(|e| RpcError::invalid_params(format!("`schema` cannot be used: {e}")))?,
        ),
    };
    let package_dir = match params.get("package_dir") {
        None | Some(Value::Null) => None,
        Some(Value::String(dir)) => Some(package_dir_param(world, dir)?),
        Some(_) => return Err(RpcError::invalid_params("`package_dir` is a string")),
    };
    let bytes = serde_json::to_vec(document).map_err(|e| RpcError::internal(e.to_string()))?;
    let mut result = regia_configurator::check_bytes(&bytes, expected, published.as_ref());
    if let Some(dir) = package_dir {
        if !matches!(result, Ok(Interface::ProductPackage) | Err(_)) {
            return Err(RpcError::invalid_params(
                "`package_dir` goes with a jewelry.product_package document",
            ));
        }
        if result.is_ok() {
            let errors = package_dir_errors(&dir, document);
            if !errors.is_empty() {
                result = Err(errors);
            }
        }
    }
    let schema_id = document.get("schema").and_then(Value::as_str);
    Ok(match result {
        Ok(interface) => json!({
            "ok": true,
            "interface": interface.wire_name(),
            "schema_id": schema_id,
            "errors": [],
        }),
        Err(errors) => json!({
            "ok": false,
            "interface": expected.map(Interface::wire_name),
            "schema_id": schema_id,
            "errors": errors.iter().map(regia_configurator::ContractError::to_json).collect::<Vec<_>>(),
        }),
    })
}

/// `configurator.contract_schema { interface? }`: one interface's schema, or the list.
pub fn contract_schema(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let row = |i: Interface| {
        json!({
            "interface": i.wire_name(),
            "schema_id": i.schema_id(),
            "owned": i.owned_by_regia(),
            "format": i.format_name(),
            "page": i.owned_by_regia().then(|| format!("docs/formats/{}.md", i.format_name())),
        })
    };
    let Some(interface) = interface_param(params)? else {
        let list: Vec<Value> = Interface::ALL.into_iter().map(row).collect();
        return Ok(json!({ "interfaces": list, "reasons": reasons() }));
    };
    let mut out = row(interface);
    let schema = interface
        .schema_text()
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
        .unwrap_or(Value::Null);
    out["schema"] = schema;
    Ok(out)
}

fn reasons() -> Vec<Value> {
    regia_configurator::REASONS
        .iter()
        .map(|(reason, code, meaning)| json!({ "reason": reason, "code": code, "meaning": meaning }))
        .collect()
}

/// Hostile path strings every `configurator.*` path parameter refuses without touching the file
/// system: the seeded fuzz's picks, the escapes, and what Windows reads as a parent or a device.
#[cfg(test)]
pub(crate) fn hostile_paths() -> Vec<String> {
    [
        "\u{0}",
        "\u{0}é",
        "\u{0}../x",
        "\u{0}F0002",
        "\u{0}\u{202e}rtl",
        "pack\u{0}",
        "pack/\u{0}/x",
        "..",
        "../x",
        "a/../b",
        "/etc",
        "C:/x",
        "C:x",
        "\\\\server\\share",
        "..\\x",
        "...",
        ".. /x",
        "NUL",
        "pack/con.txt",
        "COM1/x",
    ]
    .iter()
    .map(|p| (*p).to_owned())
    .chain([format!("\u{0}{}", "z".repeat(300))])
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_fixture() -> Value {
        let text = include_str!("../../regia_configurator/tests/fixtures/jewelry-build/build-house-a.json");
        serde_json::from_str(text).expect("fixture")
    }

    #[test]
    fn a_schema_that_is_not_an_object_is_refused_not_compiled() {
        let mut world = World::new();
        for wrong in [json!("not an object"), json!(true), json!([1])] {
            let e = contract_check(
                &mut world,
                &json!({ "document": build_fixture(), "schema": wrong }),
            )
            .expect_err("refused");
            assert_eq!(e.code, crate::protocol::INVALID_PARAMS);
            assert_eq!(e.data.as_ref().map(|d| d["param"].clone()), Some(json!("schema")));
        }
        let absent = contract_check(
            &mut world,
            &json!({ "document": build_fixture(), "schema": null }),
        )
        .expect("null is absent");
        assert_eq!(absent["ok"], true, "{absent}");
    }

    #[test]
    fn contract_check_accepts_a_fixture_and_reports_refusals() {
        let mut world = World::new();
        let ok = contract_check(&mut world, &json!({ "document": build_fixture() })).expect("call");
        assert_eq!(ok["ok"], true, "{ok}");
        assert_eq!(ok["interface"], "build");
        let mut bad = build_fixture();
        bad["cost"] = json!(12.5);
        let refused =
            contract_check(&mut world, &json!({ "document": bad, "interface": "build" })).expect("call");
        assert_eq!(refused["ok"], false);
        assert_eq!(refused["errors"][0]["reason"], "contract_unknown_field");
        assert_eq!(refused["errors"][0]["code"], "F0002");
        assert_eq!(refused["errors"][0]["path"], "/cost");
        let wrong = contract_check(
            &mut world,
            &json!({ "document": build_fixture(), "interface": "jewelry.part_kit/1" }),
        )
        .expect("call");
        assert_eq!(wrong["errors"][0]["reason"], "contract_wrong_interface");
    }

    #[test]
    fn contract_check_takes_a_published_schema_for_the_kit_builders_interfaces() {
        let mut world = World::new();
        let doc = json!({ "schema": "jewelry.piece_recipe/1", "house": "house-a" });
        let none = contract_check(&mut world, &json!({ "document": doc })).expect("call");
        assert_eq!(none["errors"][0]["reason"], "contract_no_schema");
        let schema = json!({ "type": "object", "required": ["schema", "house", "view_range"] });
        let with = contract_check(&mut world, &json!({ "document": doc, "schema": schema })).expect("call");
        assert_eq!(with["errors"][0]["reason"], "contract_missing_field");
        assert_eq!(with["errors"][0]["path"], "/view_range");
        let bad = contract_check(
            &mut world,
            &json!({ "document": doc, "schema": { "unevaluatedProperties": false } }),
        );
        assert!(bad.is_err(), "an unsupported keyword refuses the schema");
    }

    /// A world whose project root holds a copy of the committed package folder.
    fn package_project() -> (World, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir("configurator-package");
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../regia_configurator/tests/fixtures/jewelry-product-package/package-basic");
        std::fs::create_dir_all(root.join("pack")).expect("dir");
        for name in [
            "mesh.rvm",
            "luts.bin",
            "recipe.json",
            "options.json",
            "materials.json",
        ] {
            std::fs::copy(source.join(name), root.join("pack").join(name)).expect("copy");
        }
        let mut world = World::new();
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("server");
        world.insert_resource(server);
        (world, root)
    }

    fn manifest() -> Value {
        let text = include_str!(
            "../../regia_configurator/tests/fixtures/jewelry-product-package/package-basic.json"
        );
        serde_json::from_str(text).expect("fixture")
    }

    #[test]
    fn contract_check_reads_a_package_folder() {
        let (mut world, root) = package_project();
        let call = |world: &mut World, doc: Value, dir: &str| {
            contract_check(world, &json!({ "document": doc, "package_dir": dir })).expect("call")
        };
        let ok = call(&mut world, manifest(), "pack");
        assert_eq!(ok["ok"], true, "{ok}");
        // A symlink leaving the folder reads as absent, even with the right bytes behind it.
        #[cfg(unix)]
        {
            let outside = root.join("outside.bin");
            std::fs::copy(root.join("pack/mesh.rvm"), &outside).expect("copy");
            std::fs::remove_file(root.join("pack/mesh.rvm")).expect("remove");
            std::os::unix::fs::symlink(&outside, root.join("pack/mesh.rvm")).expect("link");
            let linked = call(&mut world, manifest(), "pack");
            assert_eq!(linked["errors"][0]["reason"], "contract_package_file_missing");
            std::fs::remove_file(root.join("pack/mesh.rvm")).expect("remove");
            std::fs::rename(&outside, root.join("pack/mesh.rvm")).expect("restore");
        }
        // A changed file is refused, then an absent one.
        std::fs::write(root.join("pack/mesh.rvm"), b"changed").expect("write");
        let changed = call(&mut world, manifest(), "pack");
        assert_eq!(changed["ok"], false);
        assert_eq!(changed["errors"][0]["reason"], "contract_package_file_mismatch");
        assert_eq!(changed["errors"][0]["code"], "F0002");
        std::fs::remove_file(root.join("pack/mesh.rvm")).expect("remove");
        let absent = call(&mut world, manifest(), "pack");
        assert_eq!(absent["errors"][0]["reason"], "contract_package_file_missing");
        assert_eq!(absent["errors"][0]["code"], "F0003");
        // A folder outside the project, another interface and a wrong type are bad parameters.
        let outside = contract_check(
            &mut world,
            &json!({ "document": manifest(), "package_dir": "../elsewhere" }),
        );
        assert!(outside.is_err());
        let other = contract_check(
            &mut world,
            &json!({ "document": build_fixture(), "package_dir": "pack" }),
        );
        assert!(other.is_err(), "package_dir goes with a package");
        let wrong = contract_check(&mut world, &json!({ "document": manifest(), "package_dir": 3 }));
        assert!(wrong.is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    fn error_code(e: &RpcError) -> &str {
        e.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
    }

    #[test]
    fn package_dir_refuses_hostile_paths_before_the_file_system() {
        let (mut world, root) = package_project();
        let check = |world: &mut World, dir: &str| {
            contract_check(world, &json!({ "document": manifest(), "package_dir": dir }))
        };
        let mut hostile = hostile_paths();
        hostile.extend(["", ".", "./"].map(str::to_owned));
        for dir in &hostile {
            let e = check(&mut world, dir).expect_err(dir);
            assert_eq!(error_code(&e), "F0002", "{dir:?}: {e:?}");
            let data = e.data.as_ref().expect("data");
            assert_eq!(data["reason"], "path_outside_project", "{dir:?}: {e:?}");
            assert!(data["path_reason"].is_string(), "{dir:?}: {e:?}");
            assert!(!e.message.contains("os error"), "{dir:?}: {}", e.message);
        }
        // A trailing slash still names the folder.
        assert_eq!(check(&mut world, "pack/").expect("call")["ok"], true);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn package_dir_answers_its_codes_without_os_text() {
        let (mut world, root) = package_project();
        let check = |world: &mut World, dir: &str| {
            contract_check(world, &json!({ "document": manifest(), "package_dir": dir })).expect_err(dir)
        };
        // Absent: below a missing folder and below a file.
        for dir in ["missing", "missing/deeper", "pack/mesh.rvm/x"] {
            let e = check(&mut world, dir);
            assert_eq!(error_code(&e), "F0003", "{dir}: {e:?}");
            assert!(!e.message.contains("os error"), "{dir}: {}", e.message);
        }
        // A file is not a folder (it used to answer every listed file as missing).
        let e = check(&mut world, "pack/mesh.rvm");
        assert_eq!(error_code(&e), "F0002", "{e:?}");
        // Over-long: a segment the file system cannot hold is refused. Past the whole path limit,
        // the walk stops at the first absent folder, so the answer is that it does not exist.
        #[cfg(unix)]
        for (dir, want) in [("z".repeat(300), "F0002"), ("a/".repeat(3000), "F0003")] {
            let e = check(&mut world, &dir);
            assert_eq!(error_code(&e), want, "{e:?}");
            assert!(!e.message.contains("os error"), "{}", e.message);
        }
        // A symlink that leads out is refused as one.
        #[cfg(unix)]
        {
            let outside = regia_test_utils::unique_temp_dir("configurator-package-outside");
            std::os::unix::fs::symlink(&outside, root.join("out")).expect("link");
            let e = check(&mut world, "out");
            let data = e.data.as_ref().expect("data");
            assert_eq!(data["path_reason"], "path_escapes", "{e:?}");
            std::fs::remove_dir_all(&outside).ok();
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// The walk behind the macOS answer past the path limit (ADR-5550), on any host: an absent
    /// folder early in an over-long path is found; an over-long segment and a whole existing path
    /// are not absent folders.
    #[test]
    fn an_absent_folder_is_found_before_the_path_limit() {
        let root = regia_test_utils::unique_temp_dir("configurator-absent-walk");
        assert!(has_absent_folder(&root.join("a/".repeat(3000))));
        assert!(has_absent_folder(&root.join("missing")));
        assert!(!has_absent_folder(&root));
        #[cfg(unix)]
        assert!(!has_absent_folder(&root.join("z".repeat(300))));
        std::fs::remove_dir_all(&root).ok();
    }

    /// A manifest's own file names go through the same rule, and only a regular file is read.
    #[test]
    fn package_files_read_only_regular_files_inside_the_folder() {
        let (world, root) = package_project();
        #[cfg(not(unix))]
        drop(world);
        let dir = root.join("pack").canonicalize().expect("dir");
        for name in ["../recipe.json", "/etc/hostname", "C:/x", "a\u{0}b", "NUL"] {
            let doc = json!({ "files": [{ "name": name, "size": 0, "blake3": "" }] });
            let errors = package_dir_errors(&dir, &doc);
            assert_eq!(errors[0].reason, "contract_package_file_missing", "{name}");
        }
        std::fs::create_dir(dir.join("sub")).expect("dir");
        let doc = json!({ "files": [{ "name": "sub", "size": 0, "blake3": "" }] });
        assert_eq!(
            package_dir_errors(&dir, &doc)[0].reason,
            "contract_package_file_missing"
        );
        // A FIFO would block the agent's thread on open. The writer lets a regression fail here
        // instead of hanging: it unblocks the read, which then reports a mismatch.
        #[cfg(unix)]
        {
            let mut world = world;
            let fifo = dir.join("mesh.rvm");
            std::fs::remove_file(&fifo).expect("remove");
            let made = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo");
            assert!(made.success());
            let writer = fifo.clone();
            std::thread::spawn(move || std::fs::write(writer, b"x"));
            let answer = contract_check(
                &mut world,
                &json!({ "document": manifest(), "package_dir": "pack" }),
            )
            .expect("call");
            assert_eq!(
                answer["errors"][0]["reason"], "contract_package_file_missing",
                "{answer}"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// `regia_configurator` has no engine dependency, so `clean_path` holds its own copy of the
    /// request-path rule: whatever the rule refuses, it must refuse too.
    #[test]
    fn clean_path_refuses_what_the_request_path_rule_refuses() {
        let mut paths = hostile_paths();
        paths.extend(
            [
                "",
                ".",
                "./",
                "a\\b",
                "\\x",
                ". .",
                ".. ",
                "x/...",
                "aux",
                "PRN.json",
                "con ",
                "CONIN$",
                "conout$/a",
                "lpt9.bin",
            ]
            .map(str::to_owned),
        );
        for path in &paths {
            if regia_core::project_path::check_relative(path).is_err() {
                assert!(!regia_configurator::source::clean_path(path), "{path:?}");
            }
        }
    }

    #[test]
    fn contract_check_params() {
        let mut world = World::new();
        assert!(contract_check(&mut world, &json!({})).is_err());
        assert!(contract_check(&mut world, &json!({ "document": {}, "interface": "nope" })).is_err());
        assert!(contract_check(&mut world, &json!({ "document": {}, "interface": 3 })).is_err());
        let r = contract_check(&mut world, &json!({ "document": [1, 2] })).expect("call");
        assert_eq!(r["errors"][0]["reason"], "contract_no_schema_field");
    }

    #[test]
    fn contract_schema_lists_and_returns_schemas() {
        let mut world = World::new();
        let list = contract_schema(&mut world, &json!({})).expect("call");
        assert_eq!(
            list["interfaces"].as_array().map(Vec::len),
            Some(Interface::ALL.len())
        );
        assert!(
            list["reasons"]
                .as_array()
                .is_some_and(|r| r.len() == regia_configurator::REASONS.len())
        );
        for name in ["build", "jewelry.build/1", "jewelry-build"] {
            let one = contract_schema(&mut world, &json!({ "interface": name })).expect("call");
            assert_eq!(
                one["schema"]["properties"]["schema"]["const"], "jewelry.build/1",
                "{name}"
            );
            assert_eq!(one["page"], "docs/formats/jewelry-build.md");
        }
        let other = contract_schema(&mut world, &json!({ "interface": "option_graph" })).expect("call");
        assert_eq!(other["owned"], false);
        assert!(other["schema"].is_null());
    }
}
