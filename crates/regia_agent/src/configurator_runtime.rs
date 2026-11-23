// SPDX-License-Identifier: MPL-2.0
//! `configurator.open_product`, `configurator.options`, `configurator.set_pick`,
//! `configurator.build` and `configurator.resolve`: the product configurator runtime
//! (ADR-2534, `book/agents/configurator.md`).
//!
//! One product is open at a time (a [`ConfiguratorSession`] resource). Every refusal answers with
//! a lowercase `configurator_*` code; the configurator never holds a build the option graph does
//! not sell.

use regia_app::prelude::*;
use regia_configurator::resolve::Pick;
use regia_configurator::runtime::{BuildOptions, Camera, ConfigError, Configurator, SetMode, Tenant};
use regia_configurator::source::{
    ContentSource, DirSource, MemorySource, OpenRequest, clean_root, load_product,
};
use serde_json::{Value, json};

use crate::protocol::{INTERNAL_ERROR, INVALID_PARAMS, NOT_FOUND, RpcError};

/// The open product, if any.
#[derive(Resource, Default)]
pub struct ConfiguratorSession {
    current: Option<Configurator>,
}

const HINT: &str = "see book/agents/configurator.md";

fn refusal(e: &ConfigError) -> RpcError {
    let class = match e.reason {
        "configurator_missing_file" | "configurator_not_open" => NOT_FOUND,
        "configurator_build_invalid" => INTERNAL_ERROR,
        _ => INVALID_PARAMS,
    };
    RpcError::new(class, e.reason, e.message.clone(), HINT)
}

fn not_open() -> RpcError {
    RpcError::new(
        NOT_FOUND,
        "configurator_not_open",
        "no product is open",
        "call configurator.open_product first",
    )
}

fn str_param<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(RpcError::invalid_params(format!("`{name}` is a string"))),
    }
}

fn strings(params: &Value, name: &str) -> Result<Vec<String>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| RpcError::invalid_params(format!("`{name}` is a list of strings")))
            })
            .collect(),
        Some(_) => Err(RpcError::invalid_params(format!("`{name}` is a list of strings"))),
    }
}

fn to_value<T: serde::Serialize>(x: &T) -> Value {
    serde_json::to_value(x).unwrap_or(Value::Null)
}

/// The state an answer reports after a change: picks, resolution, listing and look.
fn state(c: &Configurator) -> Value {
    let product = c.product();
    json!({
        "product_key": product.product_key,
        "listing": if product.is_incomplete() { "incomplete" } else { "complete" },
        "flags": product.gap_codes(),
        "picks": to_value(c.picks()),
        "resolution": c.resolve().to_agent_json(),
        "geometry_key": c.geometry_key(),
        "camera": to_value(c.camera()),
        "look": to_value(&c.look()),
    })
}

/// `configurator.open_product { product_key, root? | documents?, snapshot_id?, library_version?,
/// option_graph?, recipe?, library?, kits?, tenant_id?, tenant_houses?, validate?, build? }`.
pub fn open_product(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(product_key) = str_param(params, "product_key")? else {
        return Err(RpcError::invalid_params("`product_key` is required"));
    };
    let validate = match params.get("validate") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(RpcError::invalid_params("`validate` is a boolean")),
    };
    let mut request = OpenRequest {
        product_key: product_key.to_owned(),
        snapshot_id: str_param(params, "snapshot_id")?.map(str::to_owned),
        library_version: str_param(params, "library_version")?.map(str::to_owned),
        option_graph_path: str_param(params, "option_graph")?.map(str::to_owned),
        recipe_path: str_param(params, "recipe")?.map(str::to_owned),
        library_path: str_param(params, "library")?.map(str::to_owned),
        kit_paths: strings(params, "kits")?,
        validate,
    };
    let source: Box<dyn ContentSource> = match (params.get("documents"), str_param(params, "root")?) {
        (Some(Value::Object(docs)), None) => {
            // Inline documents: each goes under a fixed path, and the request names them.
            let mut mem = MemorySource::new();
            let mut put = |path: String, doc: &Value| -> Result<String, RpcError> {
                let bytes = serde_json::to_vec(doc).map_err(|e| RpcError::internal(e.to_string()))?;
                mem.insert(path.clone(), bytes);
                Ok(path)
            };
            let Some(graph) = docs.get("option_graph") else {
                return Err(RpcError::invalid_params("`documents.option_graph` is required"));
            };
            request.option_graph_path = Some(put("inline/option_graph.json".to_owned(), graph)?);
            // Without a recipe the layout default is looked up, is absent, and the product opens
            // without a look.
            request.recipe_path = match docs.get("recipe") {
                Some(r) if !r.is_null() => Some(put("inline/recipe.json".to_owned(), r)?),
                _ => None,
            };
            if let Some(l) = docs.get("library").filter(|l| !l.is_null()) {
                request.library_path = Some(put("inline/library.json".to_owned(), l)?);
            }
            request.kit_paths.clear();
            // A wrong-typed `documents.kits` is refused, not read as no kits (ADR-2622).
            for (i, k) in crate::params::opt_array(docs, "kits")?
                .into_iter()
                .flatten()
                .enumerate()
            {
                let path = put(format!("inline/kit-{i}.json"), k)?;
                request.kit_paths.push(path);
            }
            Box::new(mem)
        }
        (None, Some(root)) => {
            if !clean_root(root) {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    "configurator_bad_path",
                    "`root` is empty, over-long or holds a control character",
                    HINT,
                ));
            }
            // `root` may name any content folder (book/agents/safety.md), but not a network or
            // device form: merely stating `\\host\share` makes Windows connect to the host and
            // offer the user's credentials (ADR-5869).
            if regia_core::project_path::is_remote_form(root) {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    "configurator_bad_path",
                    "`root` is a network or device path (`\\\\host\\share`, `//host/share`, `\\\\?\\`)",
                    HINT,
                ));
            }
            Box::new(DirSource::new(root))
        }
        (Some(_), Some(_)) => return Err(RpcError::invalid_params("give `root` or `documents`, not both")),
        (Some(_), None) => return Err(RpcError::invalid_params("`documents` is an object")),
        (None, None) => {
            return Err(RpcError::invalid_params(
                "`root` (a content directory) or `documents` is required",
            ));
        }
    };
    let content = load_product(source.as_ref(), &request).map_err(|e| refusal(&e.into()))?;
    let house = content.product.house().to_owned();
    let tenant_id = str_param(params, "tenant_id")?
        .map(str::to_owned)
        .unwrap_or_else(|| format!("house:{house}"));
    let tenant = Tenant::new(tenant_id, strings(params, "tenant_houses")?);
    let read = content.read.clone();
    let mut configurator = Configurator::open(content, tenant).map_err(|e| refusal(&e))?;
    let reopened = match params.get("build") {
        None | Some(Value::Null) => Value::Null,
        Some(build) => to_value(&configurator.load_build(build).map_err(|e| refusal(&e))?),
    };
    let mut out = state(&configurator);
    out["read"] = json!(read);
    out["reopened"] = reopened;
    out["view_range"] = json!({
        "tilt_max_deg": configurator.view_range().tilt_max_deg,
        "spin": configurator.view_range().spin,
        "rock_max_deg": configurator.view_range().rock_max_deg,
        "pan_bounds_mm": configurator.view_range().pan_bounds_mm,
        "front_axis": configurator.view_range().front_axis,
    });
    world
        .get_resource_or_insert_with(ConfiguratorSession::default)
        .current = Some(configurator);
    Ok(out)
}

fn current(world: &World) -> Result<&Configurator, RpcError> {
    world
        .get_resource::<ConfiguratorSession>()
        .and_then(|s| s.current.as_ref())
        .ok_or_else(not_open)
}

/// `configurator.options {}`.
pub fn options(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(to_value(&current(world)?.options()))
}

/// `configurator.set_pick { group, value, adjust? }`.
pub fn set_pick(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(group) = str_param(params, "group")? else {
        return Err(RpcError::invalid_params("`group` is required"));
    };
    let value = match params.get("value") {
        None => {
            return Err(RpcError::invalid_params(
                "`value` is required (null clears an optional add-on)",
            ));
        }
        Some(Value::Null) => None,
        Some(v) => Some(
            serde_json::from_value::<Pick>(v.clone())
                .map_err(|_| RpcError::invalid_params("`value` is a label, a list of labels or null"))?,
        ),
    };
    let mode = match params.get("adjust") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => SetMode::Strict,
        Some(Value::Bool(true)) => SetMode::Adjust,
        Some(_) => return Err(RpcError::invalid_params("`adjust` is a boolean")),
    };
    let Some(c) = world
        .get_resource_mut::<ConfiguratorSession>()
        .map(|s| s.into_inner())
        .and_then(|s| s.current.as_mut())
    else {
        return Err(not_open());
    };
    let outcome = c.set_pick(group, value, mode).map_err(|e| refusal(&e))?;
    let mut out = state(c);
    out["dropped"] = to_value(&outcome.dropped);
    out["filled"] = to_value(&outcome.filled);
    Ok(out)
}

/// `configurator.build { camera?, wear_preset?, studio_id?, output_gamut?, content_build_id?, build_id? }`.
pub fn build(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let camera = match params.get("camera") {
        None | Some(Value::Null) => None,
        Some(v) => Some(serde_json::from_value::<Camera>(v.clone()).map_err(|e| {
            RpcError::invalid_params(format!("`camera` is a jewelry.build/1 camera object ({e})"))
        })?),
    };
    let wear_preset = str_param(params, "wear_preset")?.map(str::to_owned);
    let mut options = BuildOptions {
        camera,
        ..BuildOptions::default()
    };
    if let Some(s) = str_param(params, "studio_id")? {
        options.studio_id = s.to_owned();
    }
    if let Some(s) = str_param(params, "output_gamut")? {
        if s != "srgb" && s != "p3" {
            return Err(RpcError::invalid_params("`output_gamut` is srgb or p3"));
        }
        options.output_gamut = s.to_owned();
    }
    if let Some(s) = str_param(params, "content_build_id")? {
        options.content_build_id = s.to_owned();
    }
    options.build_id = str_param(params, "build_id")?.map(str::to_owned);
    let Some(c) = world
        .get_resource_mut::<ConfiguratorSession>()
        .map(|s| s.into_inner())
        .and_then(|s| s.current.as_mut())
    else {
        return Err(not_open());
    };
    if let Some(preset) = wear_preset {
        c.set_wear(&preset).map_err(|e| refusal(&e))?;
    }
    if let Some(cam) = &options.camera {
        c.set_camera(cam.clone());
    }
    let build = c.build(&options).map_err(|e| refusal(&e))?;
    Ok(json!({ "build": build }))
}

/// `configurator.resolve { picks? }`: the open product's resolution of `picks` (default: the
/// current picks). Changes nothing.
pub fn resolve(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let c = current(world)?;
    let resolution = match params.get("picks") {
        None | Some(Value::Null) => c.resolve(),
        Some(p) => {
            let picks = serde_json::from_value(p.clone()).map_err(|_| {
                RpcError::invalid_params("`picks` maps a group to a label or a list of labels")
            })?;
            regia_configurator::resolve(c.product(), &picks)
        }
    };
    let mut out = resolution.to_agent_json();
    out["listing"] = json!(if c.product().is_incomplete() {
        "incomplete"
    } else {
        "complete"
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIX: &str = "../regia_configurator/tests/fixtures";

    fn documents() -> Value {
        let read = |p: &str| -> Value {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(FIX).join(p);
            serde_json::from_slice(&std::fs::read(&path).expect("fixture")).expect("json")
        };
        json!({
            "option_graph": read("runtime/optiongraph/2026-11-04-000000000000/houses/house-a/products/p0001.json"),
            "recipe": read("runtime/recipes/house-a/d0001.json"),
            "library": read("jewelry-material-library/library-basic.json"),
            "kits": [read("jewelry-part-kit/kit-shared-basic.json")],
        })
    }

    fn code(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_owned()
    }

    /// The committed content folder, and a request that opens from it with `extra` on top.
    fn from_fixture_root(extra: Value) -> Value {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(FIX)
            .join("runtime");
        let mut request = json!({
            "product_key": "house-a/p0001",
            "root": root.display().to_string(),
            "snapshot_id": "2026-11-04-000000000000",
        });
        if let (Some(request), Value::Object(extra)) = (request.as_object_mut(), extra) {
            request.extend(extra);
        }
        request
    }

    #[test]
    fn hostile_request_paths_answer_configurator_bad_path() {
        let mut world = World::new();
        let baseline = open_product(&mut world, &from_fixture_root(json!({}))).expect("open");
        assert_eq!(baseline["product_key"], "house-a/p0001");
        let mut hostile = crate::configurator::hostile_paths();
        hostile.extend([String::new(), "z".repeat(300), "a/".repeat(600) + "a"]);
        for path in &hostile {
            for name in [
                "option_graph",
                "recipe",
                "library",
                "snapshot_id",
                "library_version",
            ] {
                let e = open_product(&mut world, &from_fixture_root(json!({ name: path }))).expect_err(name);
                assert_eq!(code(&e), "configurator_bad_path", "{name} = {path:?}: {e:?}");
            }
            let e =
                open_product(&mut world, &from_fixture_root(json!({ "kits": [path] }))).expect_err("kits");
            assert_eq!(code(&e), "configurator_bad_path", "kits = {path:?}: {e:?}");
        }
        // A house is a path segment too.
        let e = open_product(
            &mut world,
            &from_fixture_root(json!({ "product_key": "\u{0}/p0001" })),
        )
        .expect_err("house");
        assert_eq!(code(&e), "configurator_bad_path", "{e:?}");
    }

    #[test]
    fn a_hostile_root_answers_configurator_bad_path() {
        let mut world = World::new();
        let long = "z/".repeat(2000);
        // A network or device form too (sec-review-assets-agent, ADR-5869): stating one connects
        // to the host it names on Windows.
        for root in [
            "",
            "\u{0}",
            "\u{0}é",
            "out\u{0}/x",
            "out\nx",
            long.as_str(),
            r"\\attacker.example\share",
            "//attacker.example/share",
            r"\\?\C:\content",
        ] {
            let e = open_product(&mut world, &from_fixture_root(json!({ "root": root }))).expect_err("root");
            assert_eq!(code(&e), "configurator_bad_path", "{root:?}: {e:?}");
        }
    }

    /// A wrong-typed inline `documents.kits` is refused naming the parameter, not opened as a
    /// product without kits (ADR-2622, ADR-5667); the array still opens.
    #[test]
    fn a_wrong_typed_inline_kits_is_refused() {
        let mut world = World::new();
        for kits in [json!("kit-shared-basic.json"), json!({ "kit": 1 }), json!(7)] {
            let mut docs = documents();
            docs["kits"] = kits.clone();
            let e = open_product(
                &mut world,
                &json!({ "product_key": "house-a/p0001", "documents": docs }),
            )
            .expect_err("wrong-typed kits");
            assert_eq!(code(&e), "F0002", "{kits}: {e:?}");
            assert_eq!(
                e.data.as_ref().map(|d| d["param"].clone()),
                Some(json!("kits")),
                "{kits}: {e:?}"
            );
        }
        open_product(
            &mut world,
            &json!({ "product_key": "house-a/p0001", "documents": documents() }),
        )
        .expect("kits open");
    }

    #[test]
    fn unreadable_content_answers_without_os_text() {
        let dir = regia_test_utils::unique_temp_dir("configurator-source");
        std::fs::write(dir.join("file.json"), b"{}").expect("write");
        std::fs::create_dir(dir.join("folder")).expect("dir");
        let root = dir.display().to_string();
        let mut world = World::new();
        let mut open = |root: &str, graph: &str| {
            open_product(
                &mut world,
                &json!({ "product_key": "house-a/p0001", "root": root, "option_graph": graph }),
            )
            .expect_err(graph)
        };
        let cases = [
            (root.as_str(), "absent.json", "configurator_missing_file"),
            // Below a file: nothing is there, as on a missing folder.
            (root.as_str(), "file.json/x.json", "configurator_missing_file"),
            ("/no/such/content/root", "x.json", "configurator_missing_file"),
            (root.as_str(), "folder", "configurator_io"),
        ];
        for (root, graph, want) in cases {
            let e = open(root, graph);
            assert_eq!(code(&e), want, "{graph}: {e:?}");
            assert!(!e.message.contains("os error"), "{graph}: {}", e.message);
        }
        // A device is never read: /dev/zero would run until memory ends, a FIFO would block.
        // /dev/null shows the same check and stays harmless if it regresses.
        #[cfg(unix)]
        {
            let e = open("/dev", "null");
            assert_eq!(code(&e), "configurator_io", "{e:?}");
            assert!(!e.message.contains("os error"), "{}", e.message);
        }
        // Over-long past the file system's limit, with a root the rule accepts.
        #[cfg(unix)]
        {
            let long = dir.join("z".repeat(300)).display().to_string();
            let e = open(&long, "x.json");
            assert_eq!(code(&e), "configurator_io", "{e:?}");
            assert!(!e.message.contains("os error"), "{}", e.message);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A read stops one byte past the limit, so a huge file costs that much at most and the
    /// loader still sees that it is too large.
    #[test]
    fn a_huge_file_is_read_only_past_the_limit() {
        let dir = regia_test_utils::unique_temp_dir("configurator-huge");
        let file = std::fs::File::create(dir.join("huge.json")).expect("create");
        file.set_len(regia_configurator::MAX_FILE_BYTES as u64 + 4096)
            .expect("sparse");
        drop(file);
        let bytes = DirSource::new(&dir).read("huge.json").expect("read");
        assert_eq!(bytes.len(), regia_configurator::MAX_FILE_BYTES + 1);
        let mut world = World::new();
        let e = open_product(
            &mut world,
            &json!({ "product_key": "house-a/p0001", "root": dir.display().to_string(), "option_graph": "huge.json" }),
        )
        .expect_err("huge");
        assert_eq!(code(&e), "configurator_bad_document", "{e:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_pick_build_resolve() {
        let mut world = World::new();
        assert_eq!(
            code(&options(&mut world, &json!({})).expect_err("not open")),
            "configurator_not_open"
        );
        let opened = open_product(
            &mut world,
            &json!({ "product_key": "house-a/p0001", "documents": documents(), "tenant_id": "shop:example-shop",
                     "tenant_houses": ["house-a"] }),
        )
        .expect("open");
        assert_eq!(opened["resolution"]["variant_key"], "1001");
        assert_eq!(opened["look"]["zones"]["body"]["material_id"], "grey-metal-a");
        assert_eq!(opened["listing"], "complete");
        let o = options(&mut world, &json!({})).expect("options");
        assert!(o["groups"].as_array().is_some_and(|g| g.len() == 8));

        let refused = set_pick(&mut world, &json!({ "group": "Gauge", "value": "18g" })).expect_err("strict");
        assert_eq!(code(&refused), "configurator_not_sold");
        let adjusted = set_pick(
            &mut world,
            &json!({ "group": "Gauge", "value": "18g", "adjust": true }),
        )
        .expect("adjust");
        assert_eq!(adjusted["resolution"]["variant_key"], "1006");
        assert!(adjusted["dropped"].get("Threading").is_some());

        let b = build(
            &mut world,
            &json!({ "wear_preset": "flipped", "output_gamut": "p3" }),
        )
        .expect("build");
        assert_eq!(b["build"]["schema"], "jewelry.build/1");
        assert_eq!(b["build"]["wear"]["mirror"], true);
        assert_eq!(
            code(&build(&mut world, &json!({ "wear_preset": "nope" })).expect_err("wear")),
            "configurator_unknown_wear"
        );

        let r = resolve(&mut world, &json!({ "picks": { "Material": "Metal A" } })).expect("resolve");
        assert_eq!(
            r["reasons"],
            json!([{ "code": "axis_open", "group": "Threading" }, { "code": "axis_open", "group": "Gem Size" }])
        );
        assert_eq!(r["statement"], Value::Null);
        assert!(r["details"][0]["message"].is_string());
        assert_eq!(r["open"], json!(["Threading", "Gem Size"]));

        // Reopen the saved build.
        let saved = b["build"].clone();
        let back = open_product(
            &mut world,
            &json!({ "product_key": "house-a/p0001", "documents": documents(), "tenant_id": "shop:example-shop",
                     "tenant_houses": ["house-a"], "build": saved }),
        )
        .expect("reopen");
        assert_eq!(back["reopened"]["orderable_now"], true);
        assert_eq!(back["resolution"]["variant_key"], "1006");
    }

    #[test]
    fn refusals_carry_codes() {
        let mut world = World::new();
        let e = open_product(
            &mut world,
            &json!({ "product_key": "house-a/p0001", "documents": documents(),
                                                 "tenant_id": "house:house-b" }),
        )
        .expect_err("tenant");
        assert_eq!(code(&e), "configurator_house_not_allowed");
        let e = open_product(
            &mut world,
            &json!({ "product_key": "house-a/p0001", "root": "/nonexistent-regia-dir",
                                                 "snapshot_id": "2026-11-04-000000000000" }),
        )
        .expect_err("missing");
        assert_eq!(code(&e), "configurator_missing_file");
        let e = open_product(&mut world, &json!({ "product_key": "house-a/p0001" })).expect_err("no source");
        assert_eq!(code(&e), "F0002");
        open_product(
            &mut world,
            &json!({ "product_key": "house-a/p0001", "documents": documents() }),
        )
        .expect("open");
        for (params, want) in [
            (
                json!({ "group": "Colour", "value": "Teal" }),
                "configurator_unknown_group",
            ),
            (
                json!({ "group": "Material", "value": "Metal C" }),
                "configurator_value_unavailable",
            ),
            (
                json!({ "group": "Material", "value": "Metal Z" }),
                "configurator_unknown_value",
            ),
            (
                json!({ "group": "Extras", "value": "Sandblast" }),
                "configurator_pick_shape",
            ),
            (
                json!({ "group": "Gem", "value": null }),
                "configurator_pick_required",
            ),
            (json!({ "group": "Gem", "value": 3 }), "F0002"),
        ] {
            assert_eq!(
                code(&set_pick(&mut world, &params).expect_err("refused")),
                want,
                "{params}"
            );
        }
        // Inline documents without a recipe open without a look, and cannot build.
        let mut docs = documents();
        docs["recipe"] = Value::Null;
        open_product(
            &mut world,
            &json!({ "product_key": "house-a/p0001", "documents": docs }),
        )
        .expect("no recipe");
        assert_eq!(
            code(&build(&mut world, &json!({})).expect_err("no recipe")),
            "configurator_no_recipe"
        );
    }
}
