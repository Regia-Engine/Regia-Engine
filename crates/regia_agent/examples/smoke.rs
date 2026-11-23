// SPDX-License-Identifier: MPL-2.0
//! Protocol v0 acceptance check (specs/23 § 8 M2): drives a running `hello_window` through the agent
//! protocol and verifies every v0 method.
//!
//! ```sh
//! cargo run -p hello_window &            # or any app whose project dir is given below
//! cargo run -p regia_agent --example smoke -- samples/hello_window [--shots DIR]
//! ```

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use regia_agent::client::Client;
use serde_json::{Value, json};

struct Checks {
    failed: usize,
}

impl Checks {
    fn check(&mut self, name: &str, ok: bool, detail: impl std::fmt::Display) {
        if ok {
            println!("PASS  {name}: {detail}");
        } else {
            println!("FAIL  {name}: {detail}");
            self.failed += 1;
        }
    }
}

fn decode_png_centre(b64: &str) -> Option<[u8; 4]> {
    let bytes = base64_decode(b64)?;
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let i = ((h / 2) * w + w / 2) * 4;
    Some([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]])
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let bytes: Vec<u8> = s.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

fn run(project: PathBuf, shots: Option<PathBuf>) -> Result<usize, String> {
    let mut client = Client::connect_project(&project).map_err(|e| e.to_string())?;
    let mut c = Checks { failed: 0 };
    let mut call = |m: &str, p: Value| client.call(m, p).map_err(|e| format!("{m}: {e}"));

    let hello = call("agent.hello", json!({}))?;
    c.check(
        "agent.hello",
        hello["protocol"] == regia_agent::protocol::PROTOCOL_VERSION
            && hello["methods"].as_array().map_or(0, Vec::len) >= 10,
        format!(
            "protocol {} app {} frame {}",
            hello["protocol"], hello["app"], hello["frame"]
        ),
    );

    let found = call("scene.query_entities", json!({ "name": "Primary*" }))?;
    let entity = found["entities"][0]["entity"].as_u64();
    c.check(
        "scene.query_entities",
        entity.is_some()
            && found["entities"][0]["components"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "Window")),
        format!("{}", found["entities"][0]),
    );
    let entity = entity.ok_or("no primary window entity")?;

    let got = call(
        "scene.get_entity",
        json!({ "entity": entity, "components": ["Window"] }),
    )?;
    let title = got["components"]["Window"]["value"]["title"].clone();
    c.check(
        "scene.get_entity",
        title.is_string(),
        format!("Window.title = {title}"),
    );

    let set = call(
        "scene.set_property",
        json!({ "entity": entity, "component": "Window", "path": "title", "value": "Regia Engine - set by agent" }),
    )?;
    std::thread::sleep(Duration::from_millis(200));
    let after = call(
        "scene.get_entity",
        json!({ "entity": entity, "components": ["Window"] }),
    )?;
    c.check(
        "scene.set_property",
        after["components"]["Window"]["value"]["title"] == "Regia Engine - set by agent",
        format!("before {} after {}", set["before"], set["after"]),
    );
    let bad = call(
        "scene.set_property",
        json!({ "entity": entity, "component": "Window", "path": "no_such_field", "value": 1 }),
    );
    c.check(
        "scene.set_property error",
        bad.as_ref().is_err_and(|e| e.contains("no field")),
        bad.err().unwrap_or_default(),
    );

    let shot = |call: &mut dyn FnMut(&str, Value) -> Result<Value, String>| -> Result<(Value, Option<[u8; 4]>), String> {
        let s = call("seeing.screenshot", json!({ "target": "window" }))?;
        let px = s["png_base64"].as_str().and_then(decode_png_centre);
        Ok((s, px))
    };
    let (s1, p1) = shot(&mut call)?;
    std::thread::sleep(Duration::from_millis(1500));
    let (s2, p2) = shot(&mut call)?;
    if let Some(dir) = &shots {
        let _ = std::fs::create_dir_all(dir);
        for (i, s) in [&s1, &s2].iter().enumerate() {
            if let Some(bytes) = s["png_base64"].as_str().and_then(base64_decode) {
                let _ = std::fs::write(dir.join(format!("agent-screenshot-{i}.png")), bytes);
            }
        }
    }
    c.check(
        "seeing.screenshot",
        p1.is_some() && s1["width"].as_u64().unwrap_or(0) > 0,
        format!("{}x{} center {:?}", s1["width"], s1["height"], p1),
    );
    c.check(
        "screenshot shows time-driven color",
        p1.is_some() && p2.is_some() && p1 != p2,
        format!("{p1:?} -> {p2:?} after 1.5 s"),
    );

    call("play.pause", json!({}))?;
    let t0 = call("project.info", json!({}))?["elapsed_secs"]
        .as_f64()
        .unwrap_or(-1.0);
    std::thread::sleep(Duration::from_millis(500));
    let t1 = call("project.info", json!({}))?["elapsed_secs"]
        .as_f64()
        .unwrap_or(-2.0);
    c.check(
        "play.pause freezes game time",
        (t1 - t0).abs() < 1e-9,
        format!("elapsed {t0:.4} -> {t1:.4} over 0.5 s"),
    );
    let f0 = call("agent.hello", json!({}))?["frame"].as_u64().unwrap_or(0);
    let stepped = call("play.step", json!({ "frames": 10 }))?;
    let t2 = call("project.info", json!({}))?["elapsed_secs"]
        .as_f64()
        .unwrap_or(-3.0);
    c.check(
        "play.step",
        stepped["paused"] == true && t2 > t1 && stepped["frame"].as_u64().unwrap_or(0) >= f0 + 10,
        format!(
            "frame {f0} -> {}, game time {t1:.4} -> {t2:.4}, paused again",
            stepped["frame"]
        ),
    );
    call("play.resume", json!({}))?;

    let mark = call("logs.read", json!({ "limit": 1 }))?["next"]
        .as_u64()
        .unwrap_or(0);
    call(
        "play.inject_input",
        json!({ "events": [ { "type": "key", "key": "KeyQ", "text": "q", "state": "pressed" }, { "type": "key", "key": "KeyQ", "text": "q", "state": "released" } ] }),
    )?;
    std::thread::sleep(Duration::from_millis(300));
    let logs = call("logs.read", json!({ "since": mark, "contains": "KeyQ" }))?;
    let n = logs["records"].as_array().map_or(0, Vec::len);
    c.check(
        "play.inject_input + logs.read",
        n >= 2,
        format!("{n} matching log records since seq {mark}"),
    );
    let life = call("logs.read", json!({ "contains": "regia lifecycle: Resumed" }))?;
    c.check(
        "logs.read history",
        life["records"].as_array().is_some_and(|a| !a.is_empty()),
        format!(
            "{} lifecycle records",
            life["records"].as_array().map_or(0, Vec::len)
        ),
    );

    Ok(c.failed)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let project = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| ".".into()));
    let shots = args
        .iter()
        .position(|a| a == "--shots")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    match run(project, shots) {
        Ok(0) => {
            println!("agent smoke: PASS");
            ExitCode::SUCCESS
        }
        Ok(n) => {
            println!("agent smoke: FAIL ({n} check(s))");
            ExitCode::FAILURE
        }
        Err(e) => {
            println!("agent smoke: ERROR {e}");
            ExitCode::FAILURE
        }
    }
}
