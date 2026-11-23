// SPDX-License-Identifier: MPL-2.0
//! A render job's `product` block (`.render_job.ron` v3, ADR-2757) on the real raster renderer (lavapipe on the
//! farm): a build, a camera path and an option-change track go in, numbered frames and a manifest come out, every
//! run the same. `run_product_job` and `render.out_job` in an app are the same code; the app has no renderer, since
//! a product job needs none. Skips when the machine offers no adapter.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_agent::protocol::RpcError;
use regia_agent::run_product_job;
use regia_test_utils::unique_temp_dir;
use regia_viewer::BakedEnv;
use regia_viewer_pack::cook::{self, CookOptions};
use regia_viewer_pack::standin;
use regia_web::capture::{CaptureRig, frame_hash};
use regia_web::product::PackageProduct;
use regia_web::seams::{ProductModel, ViewRequest, ViewValues};
use serde_json::{Value, json};

const SHOTS: &str = r#"(version: 2, fps: 10.0, shots: [
    (name: "a", duration: 0.5, camera: (path: (interp: Linear,
        keys: [(time: 0.0, position: (0.0, 0.0, 60.0)), (time: 0.5, position: (5.0, 0.0, 60.0))],
        look_at: Point((0.0, 0.0, 0.0))), fov: [(time: 0.0, value: 12.0)])),
])"#;

/// A project with the stand-in marquise as `products/a` and its first build as `capture/a.build.json`.
fn project(tag: &str) -> PathBuf {
    let root = unique_temp_dir(&format!("render-out-product-{tag}"));
    let set = standin::write(&root.join("in")).expect("stand-ins");
    let (recipe, graph) = set
        .recipes
        .iter()
        .zip(&set.option_graphs)
        .find(|(recipe, _)| {
            recipe
                .file_stem()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("marquise"))
        })
        .expect("a marquise stand-in");
    let options = CookOptions {
        option_graph: Some(graph.clone()),
        materials: Some(set.library.clone()),
        ..CookOptions::default()
    };
    cook::pack(&set.kit_dir, recipe, &root.join("products").join("a"), &options).expect("cooks");
    let package = regia_viewer_pack::read_dir(&root.join("products").join("a")).expect("package");
    let mut product = PackageProduct::open(package, "studio-default").expect("opens");
    product
        .set_pick("Gem", &json!("Blue Sapphire (synthetic)"), false)
        .expect("sold");
    let view = ViewValues {
        tilt: [3.0, 4.0],
        ..ViewValues::default()
    };
    let build = product.build(&view).expect("a build");
    std::fs::create_dir_all(root.join("capture")).expect("capture dir");
    std::fs::write(
        root.join("capture").join("a.build.json"),
        serde_json::to_vec_pretty(&build).expect("json"),
    )
    .expect("build file");
    std::fs::write(root.join("capture").join("move.shots.ron"), SHOTS).expect("shots file");
    root
}

/// A job at 64x64 and 10 fps with `product` fields added to the package, and `takes` over the frames.
fn job(fields: &str, extra: &str, takes: &str) -> String {
    format!(
        r#"(version: 3, name: "swap", out: "out", {extra}
            output: (width: Some(64), height: Some(64), fps: Some(10), writers: Some(1)),
            takes: [{takes}],
            product: Some((package: "products/a", {fields})))"#
    )
}

const TRACK: &str = r#"build: Some("capture/a.build.json"),
    options: [
        (at: Frame(2), group: "Material", value: One("14K Rose Gold")),
        (at: Seconds(0.4), group: "Gem Size", value: One("4x2mm")),
    ],
    view: [(at: Frame(4), tilt: Some((10.0, -6.0)), zoom: Some(1.2), ease: Some("linear"))],"#;

const ALL: &str = r#"(name: "all", range: Frames(start: 0, end: 6))"#;

fn run(root: &Path, text: &str) -> Result<Value, RpcError> {
    run_product_job(root, text, Some("2026-11-05"), false, &mut |_| true)
}

fn code(error: &RpcError) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// `None` (test skipped) on a machine with no adapter.
fn finished(root: &Path, text: &str) -> Option<Value> {
    match run(root, text) {
        Ok(answer) => Some(answer),
        Err(error) if code(&error) == "F0003" => {
            assert!(
                std::env::var_os("REGIA_VIEWER_REQUIRE_ADAPTER").is_none(),
                "REGIA_VIEWER_REQUIRE_ADAPTER is set and there is no adapter: {error:?}"
            );
            println!("skipped: no adapter");
            None
        }
        Err(error) => panic!("refused: {error:?}"),
    }
}

fn manifest(root: &Path, answer: &Value) -> Value {
    let path = root.join(answer["manifest"].as_str().expect("a manifest"));
    serde_json::from_slice(&std::fs::read(path).expect("manifest file")).expect("manifest json")
}

fn hashes(manifest: &Value) -> Vec<String> {
    manifest["frame_hashes"]
        .as_array()
        .expect("hashes")
        .iter()
        .map(|hash| hash.as_str().expect("hash").to_owned())
        .collect()
}

fn frame_files(root: &Path, manifest: &Value, take: usize) -> Vec<Vec<u8>> {
    manifest["job"]["takes"][take]["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| std::fs::read(root.join(file.as_str().expect("file"))).expect("a frame file"))
        .collect()
}

#[test]
fn a_product_job_writes_the_same_frames_every_run() {
    let root = project("repeat");
    let text = job(TRACK, "", ALL);
    let Some(first) = finished(&root, &text) else {
        return;
    };
    assert_eq!(first["status"], json!("done"), "{first}");
    assert_eq!(first["frame"], json!(6));
    let one = manifest(&root, &first);
    let one_files = frame_files(&root, &one, 0);
    assert_eq!(one_files.len(), 6);

    let again = run(&root, &text).expect("the same job");
    let two = manifest(&root, &again);
    assert_eq!(hashes(&one), hashes(&two), "byte-identical frames");
    assert_eq!(one_files, frame_files(&root, &two, 0), "byte-identical files");

    let list = hashes(&one);
    assert_ne!(list[1], list[2], "the metal pick lands on frame 2");
    assert_ne!(list[3], list[4], "the size pick lands on frame 4");
    assert_ne!(list[0], list[1], "the view moves");

    assert_eq!(one["schema"], json!("jewelry.render_out/1"));
    assert_eq!(one["extends"], json!("regia.render_out/2"));
    let product = &one["product"];
    assert_eq!(product["renderer_used"], json!("regia_viewer"));
    assert_eq!(product["quality"], json!("raster"));
    assert!(product["package_hash"].is_string(), "{product}");
    assert_eq!(product["identity"]["house"], json!("house-a"));
    assert_eq!(
        product["identity"]["product_key"],
        json!("house-a/standin-marquise-bezel-end")
    );
    assert_eq!(
        product["identity"]["start_picks"]["Gem"],
        json!("Blue Sapphire (synthetic)"),
        "the build starts the timeline"
    );
    assert_eq!(product["option_changes"][0]["frame"], json!(2));
    assert_eq!(product["option_changes"][1]["frame"], json!(4));
    assert_eq!(product["option_changes"][1]["orderable"], json!(true));
    assert_eq!(product["camera"].as_array().map(Vec::len), Some(6));
    assert_ne!(product["camera"][0]["eye_mm"], product["camera"][4]["eye_mm"]);
    assert_eq!(
        product["camera"][4]["eye_mm"], product["camera"][5]["eye_mm"],
        "holds after the last key"
    );
    assert!(product["adapter"].is_string(), "{product}");
    assert!(
        !one.to_string().contains("cost"),
        "a manifest says nothing about money"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_job_frame_is_what_the_player_draws_for_the_same_calls() {
    let root = project("same");
    let text = job(TRACK, "", ALL);
    let Some(answer) = finished(&root, &text) else {
        return;
    };
    let from_job = hashes(&manifest(&root, &answer));

    let package = regia_viewer_pack::read_dir(&root.join("products").join("a")).expect("package");
    let mut product = PackageProduct::open(package, "studio-default").expect("opens");
    let build: Value = serde_json::from_slice(
        &std::fs::read(root.join("capture").join("a.build.json")).expect("build file"),
    )
    .expect("json");
    let (_, start) = product.load_build(&build).expect("its own build");
    let mut rig =
        CaptureRig::new(product, [64, 64], [64, 64], &BakedEnv::studio_default()).expect("a device");
    let mut from_player = Vec::new();
    for frame in 0..6_u64 {
        let t = (frame as f64 / 4.0).min(1.0);
        let mix = |a: f32, b: f32| (f64::from(a) + (f64::from(b) - f64::from(a)) * t) as f32;
        let player = rig.player();
        if frame == 2 {
            player
                .set_pick("Material", &json!("14K Rose Gold"), false)
                .expect("sold");
        }
        if frame == 4 {
            player.set_pick("Gem Size", &json!("4x2mm"), false).expect("sold");
        }
        player
            .set_view(&ViewRequest {
                tilt: Some([mix(start.tilt[0], 10.0), mix(start.tilt[1], -6.0)]),
                spin: Some(start.spin),
                pan: Some(start.pan),
                zoom: Some(mix(start.zoom, 1.2)),
                animate: Some(false),
            })
            .expect("view");
        let drawn = rig.draw(100.0).expect("a frame");
        from_player.push(frame_hash(&drawn.still.rgba));
    }
    assert_eq!(from_job, from_player, "the same pixels as the web player's core");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_take_of_a_timeline_is_those_frames_of_the_whole_timeline() {
    let root = project("takes");
    let two = r#"(name: "all", range: Frames(start: 0, end: 6)), (name: "tail", range: Seconds(start: 0.4, end: 0.6), path: Some("tail/{take}_{frame}"))"#;
    let Some(answer) = finished(&root, &job(TRACK, "", two)) else {
        return;
    };
    let block = manifest(&root, &answer);
    let takes = &block["job"]["takes"];
    assert_eq!(takes[1]["start_frame"], json!(4));
    assert_eq!(takes[1]["frames"], json!(2));
    assert_eq!(takes[1]["frame_hashes"][0], block["frame_hashes"][4]);
    assert_eq!(takes[1]["frame_hashes"][1], block["frame_hashes"][5]);
    assert_eq!(frame_files(&root, &block, 1)[0], frame_files(&root, &block, 0)[4]);
    assert!(
        takes[1]["files"][0]
            .as_str()
            .is_some_and(|file| file.ends_with("tail/tail_000004_000000.png")),
        "{}",
        takes[1]["files"][0]
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_shot_list_is_the_camera_path_in_mm() {
    let root = project("shots");
    let text = job(
        "",
        r#"shots: Some("capture/move.shots.ron"),"#,
        r#"(name: "all", range: Whole)"#,
    );
    let Some(answer) = finished(&root, &text) else {
        return;
    };
    let block = manifest(&root, &answer);
    let frames = block["delivered"].as_u64().expect("frames");
    assert_eq!(frames, 5, "0.5 s at 10 fps");
    let camera = &block["product"]["camera"];
    let eye = |frame: usize, axis: usize| camera[frame]["eye_mm"][axis].as_f64().expect("a number");
    assert!(eye(0, 0).abs() < 1e-3, "{camera}");
    assert!(
        (eye(2, 0) - 2.0).abs() < 1e-3,
        "2/5 of the way along the path: {camera}"
    );
    assert!((eye(0, 2) - 60.0).abs() < 1e-3, "{camera}");
    let list = hashes(&block);
    assert_ne!(list[0], list[4], "the camera moved");
    // The camera path repeats too.
    let again = run(&root, &text).expect("again");
    assert_eq!(hashes(&manifest(&root, &again)), list);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn whole_runs_to_the_last_timed_event_and_a_marker_times_a_pick() {
    let root = project("whole");
    std::fs::write(
        root.join("capture").join("marks.seq.ron"),
        r#"(version: 1, markers: [(name: "swap", time: 0.5), (name: "end", time: 0.75)])"#,
    )
    .expect("sequence");
    let fields = r#"options: [(at: Marker("swap"), group: "Material", value: One("Titanium"))],
        view: [(at: Frame(5), zoom: Some(1.1))],"#;
    let text = job(
        fields,
        r#"sequence: Some("capture/marks.seq.ron"),"#,
        r#"(name: "marks", range: Markers(from: "swap", to: "end"))"#,
    );
    let Some(answer) = finished(&root, &text) else {
        return;
    };
    let block = manifest(&root, &answer);
    assert_eq!(block["job"]["takes"][0]["start_frame"], json!(5));
    assert_eq!(block["product"]["option_changes"][0]["frame"], json!(5));

    let no_file = job(
        r#"view: [(at: Frame(5), zoom: Some(1.1))],"#,
        "",
        r#"(name: "all", range: Whole)"#,
    );
    let answer = run(&root, &no_file).expect("whole goes up to the last key");
    assert_eq!(answer["frame"], json!(5));
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn supersampling_keeps_the_delivered_size_and_the_limits_of_that_size() {
    let root = project("super");
    let text = r#"(version: 3, name: "ss", out: "out",
            output: (width: Some(32), height: Some(32), fps: Some(10), supersample: Some(2), writers: Some(1)),
            takes: [(name: "a", range: Frames(start: 0, end: 1))],
            product: Some((package: "products/a")))"#;
    let Some(answer) = finished(&root, text) else {
        return;
    };
    let block = manifest(&root, &answer);
    let png = frame_files(&root, &block, 0).remove(0);
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let reader = decoder.read_info().expect("a PNG");
    assert_eq!((reader.info().width, reader.info().height), (32, 32));
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn what_a_player_would_refuse_is_refused_before_a_frame_with_its_code() {
    let root = project("refuse");
    let refused = |fields: &str, extra: &str, wanted: &str, needle: &str| {
        let text = job(fields, extra, ALL);
        let error = run(&root, &text).expect_err(&text);
        assert_eq!(code(&error), wanted, "{text}: {error:?}");
        assert!(error.message.contains(needle), "{text}: {error:?}");
        assert!(
            !root.join("out").join("swap_render_out.json").exists(),
            "{text}: nothing was started"
        );
    };
    refused(
        r#"options: [(at: Frame(1), group: "Material", value: One("Moonstuff"))],"#,
        "",
        "render_job_pick_refused",
        "frame 1",
    );
    refused(
        r#"options: [(at: Frame(1), group: "Nope", value: One("x"))],"#,
        "",
        "render_job_pick_refused",
        "pick_unknown_group",
    );
    refused(
        r#"view: [(at: Frame(2), tilt: Some((70.0, 0.0)), ease: Some("hold"))],"#,
        "",
        "render_job_outside_view_range",
        "frame 2",
    );
    refused(
        r#"view: [(at: Frame(2), spin: Some(45.0), ease: Some("hold"))],"#,
        "",
        "render_job_outside_view_range",
        "frame 2",
    );
    refused(
        r#"view: [(at: Frame(2), pan: Some((50.0, 0.0)), ease: Some("hold"))],"#,
        "",
        "render_job_outside_view_range",
        "frame 2",
    );
    refused(
        r#"view: [(at: Marker("nope"))],"#,
        "",
        "render_job_no_source",
        "needs a sequence",
    );

    let mut text = job("", "", ALL);
    text = text.replace("products/a", "products/none");
    let error = run(&root, &text).expect_err("no such package");
    assert_eq!(code(&error), "render_job_bad_package", "{error:?}");

    std::fs::write(root.join("capture").join("bad.build.json"), "{").expect("file");
    refused(
        r#"build: Some("capture/bad.build.json"),"#,
        "",
        "render_job_build_refused",
        "bad.build.json",
    );
    let other = json!({"schema": "jewelry.build/1", "product_key": "elsewhere/p"});
    std::fs::write(root.join("capture").join("other.build.json"), other.to_string()).expect("file");
    refused(
        r#"build: Some("capture/other.build.json"),"#,
        "",
        "render_job_build_refused",
        "cannot open",
    );
    refused(
        r#"build: Some("capture/missing.build.json"),"#,
        "",
        "render_job_unreadable",
        "missing.build.json",
    );

    let away = std::env::temp_dir().parent().map(Path::to_path_buf);
    if let Some(away) = away {
        let text = job(
            &format!(r#"build: Some("{}/x.build.json"),"#, away.display()),
            "",
            ALL,
        );
        assert!(run(&root, &text).is_err(), "confined like `out`");
    }

    let moved = SHOTS.replace("(5.0, 0.0, 60.0)", "(500.0, 0.0, 60.0)");
    std::fs::write(root.join("capture").join("far.shots.ron"), moved).expect("file");
    let text = job(
        "",
        r#"shots: Some("capture/far.shots.ron"),"#,
        r#"(name: "all", range: Whole)"#,
    );
    let error = run(&root, &text).expect_err("past the tilt cone");
    assert_eq!(code(&error), "render_job_outside_view_range", "{error:?}");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_dry_run_answers_the_plan_with_no_gpu_and_writes_nothing() {
    let root = project("dry");
    let text = job(TRACK, "", ALL);
    let answer = run_product_job(&root, &text, None, true, &mut |_| true).expect("a plan");
    assert_eq!(answer["dry_run"], json!(true));
    let plan = &answer["plan"];
    assert_eq!(plan["frames"], json!(6));
    assert_eq!(plan["product"]["option_changes"][1]["group"], json!("Gem Size"));
    assert_eq!(plan["product"]["renderer_used"], json!("regia_viewer"));
    assert_eq!(answer["request"]["warmup_frames"], json!(0));
    assert_eq!(answer["request"]["hud"], json!(false));
    assert!(!root.join("out").exists(), "a dry run writes nothing");

    let late = job(
        r#"options: [(at: Frame(99), group: "Material", value: One("Titanium"))],"#,
        "",
        ALL,
    );
    let answer = run_product_job(&root, &late, None, true, &mut |_| true).expect("a plan");
    assert!(
        answer["plan"]["warnings"]
            .as_array()
            .is_some_and(|w| w.contains(&json!("product_option_past_end"))),
        "{answer}"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_cancelled_job_keeps_the_frames_it_made() {
    let root = project("cancel");
    let mut left = 2;
    let answer = match run_product_job(&root, &job(TRACK, "", ALL), None, false, &mut |progress| {
        if progress["frame"].as_u64().unwrap_or(0) >= 2 {
            left -= 1;
        }
        left > 0
    }) {
        Ok(answer) => answer,
        Err(error) if code(&error) == "F0003" => return,
        Err(error) => panic!("{error:?}"),
    };
    assert_eq!(answer["status"], json!("cancelled"), "{answer}");
    let block = manifest(&root, &answer);
    let delivered = block["delivered"].as_u64().expect("frames");
    assert!((2..6).contains(&delivered), "{delivered}");
    assert_eq!(
        block["product"]["camera"].as_array().map(Vec::len),
        Some(delivered as usize)
    );
    std::fs::remove_dir_all(&root).ok();
}

const CALL_TIMEOUT_MS: u64 = 60_000;
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(120);

fn call(client: &mut Client, method: &str, mut params: Value) -> Result<Value, RpcError> {
    params["timeout_ms"] = json!(CALL_TIMEOUT_MS);
    client.call(method, params).map_err(|err| match err {
        regia_agent::client::ClientError::Rpc(rpc) => rpc,
        other => panic!("{method}: {other:?}"),
    })
}

#[test]
fn render_out_job_runs_it_in_an_app_with_no_renderer() {
    let root = project("app");
    let text = job(TRACK, "", ALL);
    let Some(alone) = finished(&root, &text) else {
        return;
    };
    let alone = hashes(&manifest(&root, &alone));

    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_agent::AgentPlugin))
        .insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.clone()),
        });
    let agent_dir = endpoint::agent_dir(&root);
    let (job_text, project) = (text.clone(), root.clone());
    let script = std::thread::Builder::new()
        .name("render-out-product".into())
        .spawn(move || {
            let deadline = Instant::now() + SCRIPT_TIMEOUT;
            let token = |dir: &Path| {
                std::fs::read_to_string(dir.join("token"))
                    .map(|text| text.trim().to_owned())
                    .unwrap_or_default()
            };
            let mut client = loop {
                if let Ok((advertised, _)) = endpoint::read(&agent_dir)
                    && let Ok(client) = Client::connect(&advertised.url, &token(&agent_dir))
                {
                    break client;
                }
                assert!(Instant::now() < deadline, "no endpoint");
                std::thread::sleep(Duration::from_millis(20));
            };
            let dry = call(
                &mut client,
                "render.out_job",
                json!({"job": job_text, "dry_run": true}),
            )
            .expect("dry");
            assert_eq!(dry["plan"]["frames"], json!(6));
            let started = call(&mut client, "render.out_job", json!({"job": job_text})).expect("starts");
            let id = started["job"].as_u64().expect("a job id");
            assert_eq!(started["hud"], json!(false));
            assert_eq!(started["plan"], dry["plan"]);
            let finished = loop {
                let progress =
                    call(&mut client, "render.out_progress", json!({"job": id})).expect("progress");
                if progress["status"] != json!("running") {
                    break progress;
                }
                assert!(Instant::now() < deadline, "the job did not finish");
                std::thread::sleep(Duration::from_millis(20));
            };
            assert_eq!(finished["status"], json!("done"), "{finished}");
            let path = project.join(finished["manifest"].as_str().expect("a manifest"));
            let block: Value = serde_json::from_slice(&std::fs::read(path).expect("manifest")).expect("json");
            hashes(&block)
        })
        .expect("spawn");
    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !script.is_finished() && Instant::now() < deadline {
        app.update();
    }
    let in_app = script.join().expect("the script ran");
    assert_eq!(in_app, alone, "an app's clock and frames change nothing");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_game_job_still_needs_the_games_renderer() {
    let root = project("game");
    let error = run_product_job(
        &root,
        r#"(version: 3, out: "out", takes: [(name: "a", range: Frames(start: 0, end: 1))])"#,
        None,
        false,
        &mut |_| true,
    )
    .expect_err("no product, no renderer");
    assert_eq!(code(&error), "F0011", "{error:?}");
    std::fs::remove_dir_all(&root).ok();
}
