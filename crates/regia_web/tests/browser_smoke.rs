// SPDX-License-Identifier: MPL-2.0
//! The web player in a real headless browser: WebGPU, WebGL2 and no graphics at all.
//!
//! Needs a wasm build and a browser, so it reports a skip without them:
//! - `REGIA_WEB_DIST`: the `wasm-bindgen --target web` output folder (`cargo xtask web-player` writes one).
//! - `REGIA_HEADLESS_CHROME`: a headless Chromium binary, else the newest one under `~/.cache/ms-playwright`.
//! - `REGIA_JW_REQUIRE_BROWSER=1`: a missing piece fails instead of skipping.
//! - `REGIA_WEB_EVIDENCE`: a folder for the screenshots the page takes.
//!
//! Software rendering only (SwiftShader), so no GPU is touched. The browser runs under a watchdog, not a
//! budget: a slow machine passes. The browser never inherits a Vulkan driver override: with the farm's lavapipe
//! one it picks that driver for WebGPU instead of SwiftShader, and canvas readback comes back blank.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use regia_viewer_pack::cook::{self, CookOptions};
use regia_viewer_pack::standin;
use serde_json::Value;

const SMOKE_PAGE: &str = include_str!("smoke.html");
/// The demo page `regia viewer standin --pack` writes; the test proves it really drives this player.
const DEMO_PAGE: &str = include_str!("../../regia_viewer_pack/web/demo/index.html");
/// Added to the demo page under test: waits for the player, taps a value, reports what the page shows.
const DEMO_PROBE: &str = r##"<script type="module">
const report = async (results) => { await fetch("/result", { method: "POST", body: JSON.stringify(results) }); };
const r = [];
const check = (name, ok, detail) => r.push({ name, ok: !!ok, detail: String(detail) });
const status = document.getElementById("status");
for (let i = 0; i < 1200 && !status.textContent.startsWith("Drag to turn"); i++) await new Promise((d) => setTimeout(d, 100));
check("demo_says_ready", status.textContent.startsWith("Drag to turn"), status.textContent);
const buttons = [...document.querySelectorAll("#groups button")];
check("demo_lists_options", buttons.length >= 4, buttons.length);
const before = document.getElementById("build").textContent;
const next = buttons.find((b) => b.textContent === "14K Rose Gold");
if (next) next.click();
const after = document.getElementById("build").textContent;
check("demo_pick_changes_the_build", after !== before && after.includes("14K Rose Gold"), after.length);
const canvas = document.getElementById("view");
check("demo_canvas_is_sized", canvas.width > 1 && canvas.height > 1, canvas.width + "x" + canvas.height);
await report(r);
</script>"##;
const WATCHDOG: Duration = Duration::from_secs(600);

fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "regia-web-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

fn find_chrome() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("REGIA_HEADLESS_CHROME") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var("HOME").ok()?;
    let cache = Path::new(&home).join(".cache/ms-playwright");
    let mut found: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(cache).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("chromium_headless_shell-") {
            continue;
        }
        for sub in std::fs::read_dir(entry.path()).ok()?.flatten() {
            for binary in ["headless_shell", "chrome-headless-shell"] {
                let p = sub.path().join(binary);
                if p.is_file() {
                    found.push(p);
                }
            }
        }
    }
    found.sort();
    found.pop()
}

struct Site {
    dist: PathBuf,
    packages: PathBuf,
    damaged: PathBuf,
    evidence: Option<PathBuf>,
    result: Mutex<Option<mpsc::Sender<String>>>,
}

fn respond(stream: &mut TcpStream, status: &str, kind: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn kind_of(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("js") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("json") => "application/json",
        Some("html") => "text/html",
        _ => "application/octet-stream",
    }
}

fn serve_file(stream: &mut TcpStream, root: &Path, rel: &str) {
    let safe = !rel.split('/').any(|p| p == ".." || p.is_empty());
    match std::fs::read(root.join(rel)) {
        Ok(body) if safe => respond(stream, "200 OK", kind_of(rel), &body),
        _ => respond(stream, "404 Not Found", "text/plain", b"not found"),
    }
}

fn handle(mut stream: TcpStream, site: &Site) {
    let mut data = Vec::new();
    let mut buffer = [0u8; 65536];
    let mut header_end = None;
    let mut want = 0usize;
    loop {
        if let Some(end) = header_end
            && data.len() >= end + want
        {
            break;
        }
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => data.extend_from_slice(&buffer[..n]),
        }
        if header_end.is_none()
            && let Some(p) = data.windows(4).position(|w| w == b"\r\n\r\n")
        {
            header_end = Some(p + 4);
            let head = String::from_utf8_lossy(&data[..p]).to_ascii_lowercase();
            want = head
                .lines()
                .find_map(|l| {
                    l.strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse().ok())
                })
                .unwrap_or(0);
        }
    }
    let Some(end) = header_end else {
        return;
    };
    let head = String::from_utf8_lossy(&data[..end]).to_string();
    let body = &data[end..];
    let mut words = head.split_whitespace();
    let method = words.next().unwrap_or("");
    let target = words.next().unwrap_or("").split('?').next().unwrap_or("");
    match (method, target) {
        ("GET", "/smoke.html") => respond(&mut stream, "200 OK", "text/html", SMOKE_PAGE.as_bytes()),
        ("GET", "/demo/index.html") => {
            let page = DEMO_PAGE.replace("</body>", &format!("{DEMO_PROBE}</body>"));
            respond(&mut stream, "200 OK", "text/html", page.as_bytes());
        }
        ("GET", t) if t.starts_with("/demo/pkg/") => {
            serve_file(&mut stream, &site.dist, &t["/demo/pkg/".len()..])
        }
        ("GET", "/studio-good/env.renv") => {
            let body = regia_viewer::BakedEnv::studio_default().to_bytes();
            respond(&mut stream, "200 OK", "application/octet-stream", &body);
        }
        ("GET", "/studio-bad/env.renv") => {
            respond(&mut stream, "200 OK", "application/octet-stream", b"not a studio")
        }
        ("POST", "/result") => {
            if let Some(sender) = site.result.lock().ok().and_then(|g| g.clone()) {
                let _ = sender.send(String::from_utf8_lossy(body).to_string());
            }
            respond(&mut stream, "200 OK", "text/plain", b"ok");
        }
        ("POST", t) if t.starts_with("/evidence/") => {
            if let Some(dir) = &site.evidence {
                save_data_url(&dir.join(&t["/evidence/".len()..]), body);
            }
            respond(&mut stream, "200 OK", "text/plain", b"ok");
        }
        ("GET", t) if t.starts_with("/dist/") => serve_file(&mut stream, &site.dist, &t["/dist/".len()..]),
        ("GET", t) if t.starts_with("/packages/") => {
            serve_file(&mut stream, &site.packages, &t["/packages/".len()..])
        }
        ("GET", t) if t.starts_with("/packages-damaged/") => {
            serve_file(&mut stream, &site.damaged, &t["/packages-damaged/".len()..]);
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

/// Decodes a `data:image/png;base64,` body to a file.
fn save_data_url(path: &Path, body: &[u8]) {
    let text = String::from_utf8_lossy(body);
    let Some(b64) = text.split(',').nth(1) else {
        return;
    };
    let mut out = Vec::with_capacity(b64.len() / 4 * 3);
    let (mut bits, mut count) = (0u32, 0);
    for c in b64.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        bits = (bits << 6) | u32::from(v);
        count += 1;
        if count == 4 {
            out.extend_from_slice(&[(bits >> 16) as u8, (bits >> 8) as u8, bits as u8]);
            bits = 0;
            count = 0;
        }
    }
    if count == 3 {
        out.extend_from_slice(&[(bits >> 10) as u8, (bits >> 2) as u8]);
    } else if count == 2 {
        out.push((bits >> 4) as u8);
    }
    let _ = std::fs::write(path, out);
}

fn start_site(site: Arc<Site>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let site = site.clone();
            std::thread::spawn(move || handle(stream, &site));
        }
    });
    port
}

fn run_browser(chrome: &Path, site: &Arc<Site>, port: u16, query: &str, flags: &[&str]) -> Vec<Value> {
    let profile = temp_dir("profile");
    let mut command = Command::new(chrome);
    command
        .args([
            "--headless",
            "--no-sandbox",
            "--disable-dev-shm-usage",
            "--no-first-run",
            "--mute-audio",
            "--window-size=400,400",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .env_remove("VK_ICD_FILENAMES")
        .env_remove("VK_DRIVER_FILES")
        .args(flags)
        .arg(format!("http://127.0.0.1:{port}/{query}"));
    run_page(command, site, &profile, query)
}

/// Starts `command` (a browser opening one page of the site), waits for the page's result under the watchdog,
/// then ends the browser and removes its profile.
fn run_page(mut command: Command, site: &Arc<Site>, profile: &Path, query: &str) -> Vec<Value> {
    let (sender, receiver) = mpsc::channel();
    *site.result.lock().expect("lock") = Some(sender);
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the browser starts");
    let stderr = child.stderr.take();
    let log = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = stderr {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    });
    let got = receiver.recv_timeout(WATCHDOG);
    let _ = child.kill();
    let _ = child.wait();
    let log = log.join().unwrap_or_default();
    let _ = std::fs::remove_dir_all(profile);
    let text = got.unwrap_or_else(|_| panic!("{query}: no result within the watchdog\n{log}"));
    serde_json::from_str::<Vec<Value>>(&text).unwrap_or_else(|e| panic!("{query}: {e}: {text}"))
}

fn assert_all(query: &str, results: &[Value]) {
    let mut failed = Vec::new();
    let mut names = BTreeMap::new();
    for r in results {
        let name = r["name"].as_str().unwrap_or("?").to_string();
        let ok = r["ok"].as_bool().unwrap_or(false);
        println!(
            "{query}: {name}: {} {}",
            if ok { "ok" } else { "FAILED" },
            r["detail"].as_str().unwrap_or("")
        );
        if !ok {
            failed.push(name.clone());
        }
        names.insert(name, ok);
    }
    assert!(!results.is_empty(), "{query}: the page reported nothing");
    assert!(failed.is_empty(), "{query}: failed checks: {failed:?}");
}

/// The cooked stand-ins and a damaged copy behind a local site. Returns the site, its port, the marquise
/// product's name and the folder to remove afterwards.
fn stand_up_site(dist: PathBuf) -> (Arc<Site>, u16, String, PathBuf) {
    let root = temp_dir("smoke");
    let set = standin::write(&root.join("in")).expect("stand-ins");
    let packages = root.join("packages");
    let mut product = None;
    for (recipe, graph) in set.recipes.iter().zip(&set.option_graphs) {
        let name = recipe
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("x")
            .to_string();
        let options = CookOptions {
            option_graph: Some(graph.clone()),
            materials: Some(set.library.clone()),
            ..CookOptions::default()
        };
        cook::pack(&set.kit_dir, recipe, &packages.join(&name), &options).expect("cooks");
        if name.contains("marquise") {
            product = Some(name);
        }
    }
    let product = product.expect("the marquise stand-in");

    // A copy of the package with one flipped byte.
    let damaged = root.join("damaged");
    std::fs::create_dir_all(&damaged).expect("dir");
    for entry in std::fs::read_dir(packages.join(&product))
        .expect("package")
        .flatten()
    {
        if entry.path().is_file() {
            std::fs::copy(entry.path(), damaged.join(entry.file_name())).expect("copy");
        }
    }
    let mesh = damaged.join("mesh.rvm");
    let mut bytes = std::fs::read(&mesh).expect("mesh");
    bytes[12] ^= 0x40;
    std::fs::write(&mesh, bytes).expect("write");

    let evidence = std::env::var("REGIA_WEB_EVIDENCE").ok().map(PathBuf::from);
    if let Some(dir) = &evidence {
        std::fs::create_dir_all(dir).expect("evidence dir");
    }
    let site = Arc::new(Site {
        dist,
        packages,
        damaged,
        evidence,
        result: Mutex::new(None),
    });
    let port = start_site(site.clone());
    (site, port, product, root)
}

#[test]
fn the_player_runs_in_a_browser() {
    let required = std::env::var("REGIA_JW_REQUIRE_BROWSER").as_deref() == Ok("1");
    let dist = std::env::var("REGIA_WEB_DIST").ok().map(PathBuf::from);
    let chrome = find_chrome();
    let (Some(dist), Some(chrome)) = (dist, chrome) else {
        assert!(
            !required,
            "REGIA_JW_REQUIRE_BROWSER=1, but REGIA_WEB_DIST or a headless browser is missing"
        );
        println!(
            "skipped: set REGIA_WEB_DIST (and a browser in REGIA_HEADLESS_CHROME or ~/.cache/ms-playwright)"
        );
        return;
    };
    let module = std::fs::metadata(dist.join("regia_web_bg.wasm")).map_or(0, |m| m.len());
    println!("module under test: {} ({module} bytes)", dist.display());
    let (site, port, product, root) = stand_up_site(dist);

    const SOFT_GL: &[&str] = &[
        "--use-gl=angle",
        "--use-angle=swiftshader",
        "--enable-unsafe-swiftshader",
        "--ignore-gpu-blocklist",
    ];
    const SOFT_WEBGPU: &[&str] = &[
        "--use-gl=angle",
        "--use-angle=swiftshader",
        "--enable-unsafe-swiftshader",
        "--enable-unsafe-webgpu",
        "--use-webgpu-adapter=swiftshader",
        "--enable-features=Vulkan,WebGPU",
        "--ignore-gpu-blocklist",
    ];
    // A 2x screen: pointers and the drawing size run on device pixels, tap distances stay in CSS pixels.
    const DENSE_GL: &[&str] = &[
        "--use-gl=angle",
        "--use-angle=swiftshader",
        "--enable-unsafe-swiftshader",
        "--ignore-gpu-blocklist",
        "--force-device-scale-factor=2",
    ];
    const NOTHING: &[&str] = &[
        "--disable-gpu",
        "--disable-software-rasterizer",
        "--disable-webgl",
        "--disable-webgl2",
        "--disable-features=WebGPU",
    ];

    let p = format!("product=/packages/{product}/");
    let mut failures = Vec::new();
    for (query, flags) in [
        // `perf=1` keeps the phone test's measuring code running; the numbers mean nothing on software rendering.
        (format!("backend=webgl2&perf=1&{p}"), SOFT_GL),
        (format!("backend=auto&expect=webgl2&{p}"), SOFT_GL),
        (format!("backend=webgl2&dense=1&{p}"), DENSE_GL),
        (format!("backend=webgpu&{p}"), SOFT_WEBGPU),
        (format!("backend=auto&expect=webgpu&{p}"), SOFT_WEBGPU),
        (format!("backend=auto&expect=none&{p}"), NOTHING),
    ] {
        let results = run_browser(&chrome, &site, port, &format!("smoke.html?{query}"), flags);
        if let Err(e) = std::panic::catch_unwind(|| assert_all(&query, &results)) {
            failures.push((query, e.downcast_ref::<String>().cloned().unwrap_or_default()));
        }
    }
    let demo = format!("demo/index.html?product=/packages/{product}/");
    let results = run_browser(&chrome, &site, port, &demo, SOFT_GL);
    if let Err(e) = std::panic::catch_unwind(|| assert_all(&demo, &results)) {
        failures.push((demo, e.downcast_ref::<String>().cloned().unwrap_or_default()));
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Preferences every Firefox run gets: no first-run pages, no updates or reports, software WebGL allowed.
const FIREFOX_BASE: &[(&str, &str)] = &[
    ("browser.shell.checkDefaultBrowser", "false"),
    ("browser.aboutwelcome.enabled", "false"),
    ("browser.startup.homepage_override.mstone", "\"ignore\""),
    ("datareporting.policy.dataSubmissionEnabled", "false"),
    ("toolkit.telemetry.reportingpolicy.firstRun", "false"),
    ("app.update.disabledForTesting", "true"),
    ("browser.tabs.remote.autostart", "true"),
    ("webgl.force-enabled", "true"),
    ("media.autoplay.default", "0"),
];

/// A private X display (`Xvfb -displayfd`), ended on drop. Headless Firefox finds no GL driver for WebGL; on a
/// virtual display it uses Mesa's software one.
struct VirtualDisplay {
    child: std::process::Child,
    display: String,
}

impl VirtualDisplay {
    fn start() -> Option<Self> {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "1024x768x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut out = child.stdout.take()?;
        let mut number = String::new();
        let mut byte = [0u8; 1];
        while out.read(&mut byte).ok()? == 1 && byte[0] != b'\n' {
            number.push(char::from(byte[0]));
        }
        if number.trim().is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        Some(Self {
            child,
            display: format!(":{}", number.trim()),
        })
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Mesa's EGL vendor file: with it the GL loader never offers a GPU vendor's driver.
const MESA_EGL_VENDOR: &str = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
/// The software Vulkan driver: Firefox's WebGPU gets it whatever the environment names.
const LAVAPIPE_ICD: &str = "/usr/share/vulkan/icd.d/lvp_icd.json";

/// Opens one page of the site in Firefox on `display` with `prefs` added to a fresh profile's `user.js`.
/// Software rendering only: Mesa's llvmpipe for WebGL, a software Vulkan driver for WebGPU.
fn run_firefox(
    firefox: &Path,
    display: &VirtualDisplay,
    site: &Arc<Site>,
    port: u16,
    query: &str,
    prefs: &[(&str, &str)],
) -> Vec<Value> {
    let profile = temp_dir("firefox");
    let mut lines = String::new();
    for (key, value) in FIREFOX_BASE.iter().chain(prefs) {
        lines.push_str(&format!("user_pref(\"{key}\", {value});\n"));
    }
    std::fs::write(profile.join("user.js"), lines).expect("user.js");
    let mut command = Command::new(firefox);
    command
        .args(["--no-remote", "--window-size=400,400", "--profile"])
        .arg(&profile)
        .env("DISPLAY", &display.display)
        .env_remove("WAYLAND_DISPLAY")
        .env("MOZ_ENABLE_WAYLAND", "0")
        .env("MOZ_CRASHREPORTER_DISABLE", "1")
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .env("GALLIUM_DRIVER", "llvmpipe")
        .env("__GLX_VENDOR_LIBRARY_NAME", "mesa");
    if Path::new(MESA_EGL_VENDOR).is_file() {
        command.env("__EGL_VENDOR_LIBRARY_FILENAMES", MESA_EGL_VENDOR);
    }
    if Path::new(LAVAPIPE_ICD).is_file() {
        command
            .env("VK_ICD_FILENAMES", LAVAPIPE_ICD)
            .env("VK_DRIVER_FILES", LAVAPIPE_ICD);
    }
    command.arg(format!("http://127.0.0.1:{port}/{query}"));
    run_page(command, site, &profile, query)
}

/// The same page in Firefox (JW-M1 asks for a second browser family) on a private `Xvfb` display, software
/// rendering only: WebGL2 on llvmpipe, WebGPU on lavapipe, no graphics, a 2x screen and the demo page.
///
/// Needs `REGIA_WEB_DIST`, a Firefox binary in `REGIA_FIREFOX` and `Xvfb` on the `PATH`, so it reports a skip
/// without them; `REGIA_JW_REQUIRE_FIREFOX=1` turns a missing piece into a failure.
#[test]
fn the_player_runs_in_firefox() {
    let required = std::env::var("REGIA_JW_REQUIRE_FIREFOX").as_deref() == Ok("1");
    let dist = std::env::var("REGIA_WEB_DIST").ok().map(PathBuf::from);
    let firefox = std::env::var("REGIA_FIREFOX").ok().map(PathBuf::from);
    let (Some(dist), Some(firefox)) = (dist, firefox) else {
        assert!(
            !required,
            "REGIA_JW_REQUIRE_FIREFOX=1, but REGIA_WEB_DIST or REGIA_FIREFOX is missing"
        );
        println!("skipped: set REGIA_WEB_DIST and REGIA_FIREFOX");
        return;
    };
    let Some(display) = VirtualDisplay::start() else {
        assert!(!required, "REGIA_JW_REQUIRE_FIREFOX=1, but Xvfb did not start");
        println!("skipped: Xvfb did not start");
        return;
    };
    let (site, port, product, root) = stand_up_site(dist);
    const WEBGPU: &[(&str, &str)] = &[
        ("dom.webgpu.enabled", "true"),
        ("gfx.webgpu.ignore-blocklist", "true"),
    ];
    const DENSE: &[(&str, &str)] = &[("layout.css.devPixelsPerPx", "\"2.0\"")];
    const NOTHING: &[(&str, &str)] = &[
        ("webgl.force-enabled", "false"),
        ("webgl.disabled", "true"),
        ("dom.webgpu.enabled", "false"),
    ];
    let p = format!("product=/packages/{product}/&tag=firefox");
    let mut failures = Vec::new();
    for (query, prefs) in [
        (format!("backend=webgl2&perf=1&{p}"), &[][..]),
        (format!("backend=webgl2&dense=1&{p}"), DENSE),
        (format!("backend=webgpu&{p}"), WEBGPU),
        (format!("backend=auto&expect=webgpu&{p}"), WEBGPU),
        (format!("backend=auto&expect=none&{p}"), NOTHING),
    ] {
        let results = run_firefox(
            &firefox,
            &display,
            &site,
            port,
            &format!("smoke.html?{query}"),
            prefs,
        );
        if let Err(e) = std::panic::catch_unwind(|| assert_all(&format!("firefox {query}"), &results)) {
            failures.push((query, e.downcast_ref::<String>().cloned().unwrap_or_default()));
        }
    }
    let demo = format!("demo/index.html?product=/packages/{product}/");
    let results = run_firefox(&firefox, &display, &site, port, &demo, WEBGPU);
    if let Err(e) = std::panic::catch_unwind(|| assert_all(&format!("firefox {demo}"), &results)) {
        failures.push((demo, e.downcast_ref::<String>().cloned().unwrap_or_default()));
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(failures.is_empty(), "{failures:#?}");
}

fn adb(serial: &str, args: &[&str]) -> String {
    let output = Command::new("adb")
        .arg("-s")
        .arg(serial)
        .args(args)
        .output()
        .expect("adb runs");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The same page in a phone's Chrome, with real numbers (`docs/specs/42-product-viewer.md` § 10). Not part of a
/// normal run: it takes over the phone's screen.
///
/// ```sh
/// REGIA_WEB_DIST=<dist> REGIA_WEB_ADB_SERIAL=<serial> REGIA_WEB_BACKEND=webgl2 \
///   cargo test -p regia_web --test browser_smoke on_a_phone -- --ignored --nocapture
/// ```
#[test]
#[ignore = "needs a phone on adb, an unlocked screen and REGIA_WEB_DIST"]
fn on_a_phone() {
    let dist = PathBuf::from(std::env::var("REGIA_WEB_DIST").expect("REGIA_WEB_DIST"));
    let serial = std::env::var("REGIA_WEB_ADB_SERIAL").expect("REGIA_WEB_ADB_SERIAL");
    let backend = std::env::var("REGIA_WEB_BACKEND").unwrap_or_else(|_| "webgl2".to_string());
    let browser =
        std::env::var("REGIA_WEB_ANDROID_BROWSER").unwrap_or_else(|_| "com.android.chrome".to_string());
    let (site, port, product, root) = stand_up_site(dist);
    let (sender, receiver) = mpsc::channel();
    *site.result.lock().expect("lock") = Some(sender);

    adb(
        &serial,
        &["reverse", &format!("tcp:{port}"), &format!("tcp:{port}")],
    );
    adb(&serial, &["shell", "input", "keyevent", "KEYCODE_WAKEUP"]);
    let url =
        format!("http://127.0.0.1:{port}/smoke.html?backend={backend}&product=/packages/{product}/&perf=1");
    let started = adb(
        &serial,
        &[
            "shell",
            "am",
            "start",
            "-a",
            "android.intent.action.VIEW",
            "-d",
            &format!("'{url}'"),
            &browser,
        ],
    );
    println!("{started}");
    let got = receiver.recv_timeout(WATCHDOG);
    adb(&serial, &["shell", "input", "keyevent", "KEYCODE_HOME"]);
    adb(&serial, &["reverse", "--remove", &format!("tcp:{port}")]);
    let _ = std::fs::remove_dir_all(&root);
    let text = got.expect("the page reported back");
    let results: Vec<Value> = serde_json::from_str(&text).expect("JSON");
    assert_all(&format!("phone {backend}"), &results);
}
