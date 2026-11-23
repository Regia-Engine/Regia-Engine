// SPDX-License-Identifier: MPL-2.0
//! End-to-end checks of the dedicated-server build (spec 76 § 11 tests 1, 2 and the runtime half of
//! § 5): a throwaway copy of `samples/dedicated_server` is packaged through the real CLI, unpacked
//! and run; another fixture proves a server build that names a render type does not compile.
//!
//! The build checks are ignored by default (each compiles the server graph); the config check
//! runs always. Run them with
//! `cargo test -p regia_cli --test server_package_e2e -- --ignored --test-threads=1`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use regia_asset_pipeline::validation_rules::ValidationFile;
use regia_net::session::lifecycle::{ClientSessionState, SessionSettings};
use regia_net::session::manifest::ContentManifest;
use regia_net::session::net_session::{ConnectOptions, ConnectTarget};
use regia_net::{CryptoMode, NetSession, ServerIdentity};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn regia_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_regia"))
}

/// A throwaway workspace member: removed on drop, with the lockfile put back.
struct Member {
    dir: PathBuf,
    lock: Vec<u8>,
}

impl Member {
    fn new(name: &str) -> Self {
        let root = repo_root();
        Self {
            dir: root.join("samples").join(name),
            lock: std::fs::read(root.join("Cargo.lock")).expect("lockfile"),
        }
    }
}

impl Drop for Member {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::write(repo_root().join("Cargo.lock"), &self.lock);
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("dir");
    for entry in std::fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("entry");
        let dest = to.join(entry.file_name());
        if entry.path().is_dir() {
            if entry.file_name() != "build" && entry.file_name() != "dist" {
                copy_tree(&entry.path(), &dest);
            }
        } else {
            std::fs::copy(entry.path(), dest).expect("copy");
        }
    }
}

/// `samples/dedicated_server` under a new package name, plus presentation files no server ships.
fn game_copy(name: &str) -> Member {
    let member = Member::new(name);
    let sample = repo_root().join("samples/dedicated_server");
    copy_tree(&sample, &member.dir);
    let manifest = member.dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).expect("Cargo.toml");
    std::fs::write(
        &manifest,
        text.replace("name = \"dedicated_server\"", &format!("name = \"{name}\"")),
    )
    .expect("Cargo.toml");
    let crate_name = name.replace('-', "_");
    let main = member.dir.join("src/main.rs");
    let text = std::fs::read_to_string(&main).expect("main.rs");
    std::fs::write(
        &main,
        text.replace("dedicated_server::", &format!("{crate_name}::")),
    )
    .expect("main.rs");
    for (file, bytes) in [
        // Audio imports now measure loudness before the server content filter runs. Keep the
        // audio-exclusion assertion, using Regia's generated 1 kHz tone rather than invalid
        // placeholder bytes (regia_audio/tests/loudness_import.rs documents its provenance).
        (
            "audio/gun.ogg",
            &include_bytes!("../../regia_audio/tests/fixtures/tone_1k.ogg")[..],
        ),
        ("fonts/body.ttf", b"not really a font"),
        // A layout that loads: `regia package` validates UI layouts and fails on findings (ADR-4908),
        // and a file without `version` is v0, which never existed.
        ("ui/hud.ui.ron", b"(version: 9, root: ())"),
        ("shaders/water.wgsl", b"// nothing"),
    ] {
        let path = member.dir.join("assets").join(file);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, bytes).expect("write");
    }
    // The client plays the added sound by name, as the sample's code loads its tables: a root too,
    // or the package's orphan check fails on it. Everything else is the sample's own file, so the
    // package validates what the sample ships.
    let config = member.dir.join("config/validation.ron");
    let text = std::fs::read_to_string(&config).expect("the sample's validation config");
    std::fs::write(&config, with_audio_root(&text)).expect("validation config");
    member
}

/// The sample's `config/validation.ron` with `assets/audio/**` added first to its roots. A text
/// edit, not a re-serialization: `ValidationFile::to_ron` writes every field, and a file that names
/// `cook` keeps its own gate, so a rewritten copy would turn the package's `Fail` default into the
/// written `Warn` (ADR-4908).
fn with_audio_root(sample: &str) -> String {
    let (head, tail) = sample
        .split_once("roots: [")
        .expect("the sample's validation config lists `roots: [`");
    format!("{head}roots: [\"assets/audio/**\", {tail}")
}

/// The top-level field names of a RON struct, as the package's gate reads them.
fn top_level_fields(text: &str) -> Vec<String> {
    match ron::from_str::<ron::Value>(text) {
        Ok(ron::Value::Map(map)) => map
            .iter()
            .filter_map(|(key, _)| match key {
                ron::Value::String(name) => Some(name.clone()),
                _ => None,
            })
            .collect(),
        other => panic!("not a RON struct: {other:?}"),
    }
}

/// The copy's validation config is the sample's own file plus the audio root, field for field, and
/// names the same fields (so the package's `cook` default is the sample's). The sample roots the
/// levels and the data tables its code loads by name. Runs without a build.
#[test]
fn the_copy_keeps_the_samples_validation_config() {
    let text = std::fs::read_to_string(repo_root().join("samples/dedicated_server/config/validation.ron"))
        .expect("the sample's validation config");
    let copy_text = with_audio_root(&text);
    let sample = ValidationFile::from_ron(&text).expect("the sample's config parses");
    let copy = ValidationFile::from_ron(&copy_text).expect("the copy's config parses");
    let mut want = sample.clone();
    want.roots.insert(0, "assets/audio/**".to_owned());
    assert_eq!(copy, want);
    assert_eq!(
        top_level_fields(&copy_text),
        top_level_fields(&text),
        "the copy names the sample's fields only"
    );
    for root in ["assets/levels/**", "assets/data/**"] {
        assert!(
            sample.roots.iter().any(|r| r == root),
            "the sample roots {root}: {:?}",
            sample.roots
        );
    }
}

fn free_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .expect("udp port")
        .port()
}

fn free_tcp_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .expect("tcp port")
        .port()
}

/// `GET /health` once; `None` while the server is not up.
fn health(port: u16) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    Some(text)
}

/// Kills the process when the test fails before it stopped.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Polls `/health` until `done`, for at most 40 minutes (`regia server` compiles first). Fails the
/// test if the process exits first.
fn wait_health(child: &mut Child, port: u16, done: impl Fn(&str) -> bool) -> String {
    for _ in 0..240_000 {
        if let Some(text) = health(port)
            && done(&text)
        {
            return text;
        }
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!("the server exited early: {status}");
        }
        std::thread::yield_now();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("the server never reported the expected health");
}

fn signal(pid: u32, name: &str) {
    let status = Command::new("kill")
        .args([&format!("-{name}"), &pid.to_string()])
        .status()
        .expect("kill");
    assert!(status.success());
}

fn set_status_port(server_dir: &Path, port: u16, game_port: u16) {
    let path = server_dir.join("server.toml");
    let text = std::fs::read_to_string(&path).expect("server.toml");
    let text = text
        .replace("enabled = false", "enabled = true")
        .replace("port = 7778", &format!("port = {port}"))
        .replace("port = 7777", &format!("port = {game_port}"));
    std::fs::write(path, text).expect("server.toml");
}

/// Polls a real UDP client while the independent packaged process ticks. The deadline is only
/// an external-process hang guard; success is the session state, never an elapsed-time bound.
fn poll_packaged_client(client: &mut NetSession, done: impl Fn(ClientSessionState) -> bool) {
    let watchdog = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < watchdog {
        client.update();
        if done(client.state()) {
            return;
        }
        std::thread::yield_now();
    }
    panic!("packaged client did not reach its state: {:?}", client.state());
}

/// Exercises the content actually shipped in the archive over its encrypted UDP game port.
fn packaged_content_and_resume(server_dir: &Path, game_port: u16) {
    let mut manifest =
        ContentManifest::load(&server_dir.join("content/net-manifest.ron")).expect("packaged manifest");
    // The dedicated_server fixture registers no replicated game types. The host fills a zero
    // packaged protocol hash from the same NetPlugin registries at startup.
    if manifest.protocol_hash == 0 {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_net::NetPlugin::default());
        manifest.protocol_hash = regia_net::runtime::protocol_hash(app.world());
    }
    let key = ServerIdentity::load_or_create(&server_dir.join("server.key"))
        .expect("ephemeral packaged server identity")
        .public();
    let target = ConnectTarget::Address(([127, 0, 0, 1], game_port).into());
    let options = ConnectOptions {
        server_key: Some(key),
        ..ConnectOptions::default()
    };
    for reason in ["content_mismatch", "protocol_mismatch"] {
        let mut wrong = manifest.clone();
        if reason == "content_mismatch" {
            wrong.packs.push(("changed-gameplay".into(), 1));
            wrong.rehash();
        } else {
            wrong.protocol_hash ^= 1;
        }
        let mut client = NetSession::new(
            SessionSettings {
                encryption: CryptoMode::Required,
                ..SessionSettings::default()
            },
            wrong,
        );
        client.connect(target.clone(), options.clone()).expect("connect");
        poll_packaged_client(&mut client, |state| {
            assert_ne!(state, ClientSessionState::InGame, "a mismatched client joined");
            matches!(state, ClientSessionState::Disconnected(Some(_)))
        });
        let ClientSessionState::Disconnected(Some(got)) = client.state() else {
            panic!("missing refusal");
        };
        assert_eq!(got.code(), reason);
        println!("packaged UDP refusal: {}", got.code());
    }
    let mut client = NetSession::new(
        SessionSettings {
            encryption: CryptoMode::Required,
            ..SessionSettings::default()
        },
        manifest,
    );
    client
        .connect(target.clone(), options.clone())
        .expect("valid connect");
    poll_packaged_client(&mut client, |state| state == ClientSessionState::InGame);
    assert!(client.link_sealed());
    let peer = client.client().and_then(|c| c.peer_id()).expect("peer");
    let token = client.client().and_then(|c| c.reconnect_token()).expect("token");
    client.suspend();
    poll_packaged_client(&mut client, |state| {
        matches!(state, ClientSessionState::Disconnected(_))
    });
    client.reset();
    client
        .connect(
            target,
            ConnectOptions {
                resume: true,
                ..options
            },
        )
        .expect("resume");
    poll_packaged_client(&mut client, |state| state == ClientSessionState::InGame);
    assert!(client.link_sealed());
    assert_eq!(client.client().and_then(|c| c.peer_id()), Some(peer));
    assert_ne!(client.client().and_then(|c| c.reconnect_token()), Some(token));
    println!("packaged UDP valid client resumed in the same slot with a rotated token");
    client.suspend();
    poll_packaged_client(&mut client, |state| {
        matches!(state, ClientSessionState::Disconnected(_))
    });
}

/// Spec 76 § 11 tests 1 and 5-11 against the real binary: layout, archive, `/health`, SIGTERM drains
/// and exits 0, and the exit codes for a bad flag, a missing level and a port in use.
#[test]
#[ignore = "builds the game's server; invoke with -- --ignored"]
fn a_packaged_server_has_the_layout_and_runs() {
    let name = format!("server-e2e-{}", std::process::id());
    let member = game_copy(&name);
    let out = member.dir.join("out");

    let dry = Command::new(regia_cli())
        .args([
            "package",
            "--server",
            "--platform",
            "linux",
            "--profile",
            "dev",
            "--dry-run",
        ])
        .args(["--glibc", "host", "--project", &member.dir.to_string_lossy()])
        .args(["--out", &out.to_string_lossy()])
        .output()
        .expect("dry run");
    let text = String::from_utf8_lossy(&dry.stdout);
    assert!(
        dry.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    assert!(text.contains("levels/arena.level.ron"), "{text}");
    assert!(
        text.contains("audio/gun.ogg") && text.contains("[audio]"),
        "{text}"
    );
    assert!(!out.exists(), "a dry run writes nothing");

    let package = Command::new(regia_cli())
        .args(["package", "--server", "--platform", "linux", "--profile", "dev"])
        .args(["--glibc", "host", "--project", &member.dir.to_string_lossy()])
        .args(["--out", &out.to_string_lossy()])
        .output()
        .expect("package");
    assert!(
        package.status.success(),
        "{}{}",
        String::from_utf8_lossy(&package.stdout),
        String::from_utf8_lossy(&package.stderr)
    );
    let slug = "com.regiaengine.samples.dedicated_server-server";
    let archive = out.join(format!("{slug}-linux.tar.gz"));
    assert!(archive.is_file(), "archive at {}", archive.display());

    let extract = member.dir.join("unpacked");
    std::fs::create_dir_all(&extract).expect("dir");
    let tar = Command::new("tar")
        .args([
            "-xzf",
            &archive.to_string_lossy(),
            "-C",
            &extract.to_string_lossy(),
        ])
        .status()
        .expect("tar");
    assert!(tar.success());
    let server_dir = extract.join(slug);
    let binary = server_dir.join(format!("{name}-server"));
    for file in [
        binary.as_path(),
        &server_dir.join("content/base.fpak"),
        &server_dir.join("content/net-manifest.ron"),
        &server_dir.join("server.toml"),
        &server_dir.join("bans.ron"),
        &server_dir.join("README-server.txt"),
        &server_dir.join("assets/levels/arena.level.ron"),
        &server_dir.join("assets/data/weapons.table.ron"),
        // The lobby hub ships with every server package, and its unit starts it (KI-NT-043, ADR-8891).
        &server_dir.join("regia-lobby-hub"),
        &server_dir.join("hub.toml"),
        &server_dir.join("deploy/regia-server.service"),
        &server_dir.join("deploy/regia-hub.service"),
    ] {
        assert!(file.is_file(), "{} is in the package", file.display());
    }
    assert!(
        !server_dir.join("deploy/regia-relay.service").exists(),
        "no relay unit: no relay binary ships"
    );
    for gone in ["assets/audio", "assets/fonts", "assets/ui", "assets/shaders"] {
        assert!(!server_dir.join(gone).exists(), "{gone} stays out");
    }

    let (status_port, game_port) = (free_tcp_port(), free_udp_port());
    set_status_port(&server_dir, status_port, game_port);
    let mut server = Running(
        Command::new(&binary)
            .current_dir(&server_dir)
            .stdin(Stdio::null())
            .spawn()
            .expect("server"),
    );
    let text = wait_health(&mut server.0, status_port, |t| {
        t.contains("\"state\":\"running\"")
    });
    assert!(text.contains("levels/arena.level.ron"), "{text}");
    packaged_content_and_resume(&server_dir, game_port);
    signal(server.0.id(), "TERM");
    let status = server.0.wait().expect("wait");
    assert_eq!(status.code(), Some(0), "SIGTERM drains and exits 0");

    let code = |args: &[&str]| {
        Command::new(&binary)
            .args(args)
            .current_dir(&server_dir)
            .stdin(Stdio::null())
            .output()
            .expect("server")
            .status
            .code()
    };
    assert_eq!(code(&["--no-such-flag"]), Some(2));
    assert_eq!(code(&["--level", "levels/missing.level.ron"]), Some(4));
    let held = UdpSocket::bind(("0.0.0.0", game_port)).expect("hold the port");
    assert_eq!(code(&[]), Some(3));
    drop(held);
}

/// Spec 76 § 11 test 2: the graph has no client crate, and a fixture that names a render type under
/// the `server` feature fails to compile while its client build is fine.
#[test]
#[ignore = "checks a fixture against the server graph; invoke with -- --ignored"]
fn a_server_build_cannot_name_a_render_type() {
    let root = repo_root();
    let name = format!("server-misuse-{}", std::process::id());
    let member = Member::new(&name);
    std::fs::create_dir_all(member.dir.join("src")).expect("dir");
    std::fs::write(
        member.dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"
publish = false

[dependencies]
regia = {{ path = "../../crates/regia", default-features = false }}

[features]
default = ["regia/client"]
server = ["regia/server"]
"#
        ),
    )
    .expect("Cargo.toml");
    std::fs::write(
        member.dir.join("src/lib.rs"),
        "//! Names a render type with no `cfg`.\npub fn clear() -> regia::render::ClearColor {\n    regia::render::ClearColor::default()\n}\n",
    )
    .expect("lib.rs");

    let check = |features: &[&str]| {
        Command::new("cargo")
            .args(["check", "-p", &name])
            .args(features)
            .current_dir(&root)
            .output()
            .expect("cargo check")
    };
    let client = check(&[]);
    assert!(
        client.status.success(),
        "the client build compiles: {}",
        String::from_utf8_lossy(&client.stderr)
    );
    let server = check(&["--no-default-features", "--features", "server"]);
    assert!(!server.status.success(), "the server build must not compile");
    let text = String::from_utf8_lossy(&server.stderr);
    assert!(text.contains("render"), "{text}");

    let tree = Command::new("cargo")
        .args(["tree", "-e", "normal", "--prefix", "none", "-p", &name])
        .args(["--no-default-features", "--features", "server"])
        .current_dir(&root)
        .output()
        .expect("cargo tree");
    assert!(tree.status.success());
    let text = String::from_utf8_lossy(&tree.stdout);
    for line in text.lines() {
        let crate_name = line.split_whitespace().next().unwrap_or("");
        for bad in ["regia_render", "regia_audio", "winit", "wgpu", "kira"] {
            assert_ne!(crate_name, bad, "{line}");
        }
    }
}

/// Spec 76 § 10: `regia server --clients 1` runs the server from the dev cache and a headless client
/// joins it on loopback.
#[test]
#[ignore = "builds the server and the client; invoke with -- --ignored"]
fn the_dev_loop_joins_a_headless_client() {
    let name = format!("server-dev-{}", std::process::id());
    let member = game_copy(&name);
    let (status_port, game_port) = (free_tcp_port(), free_udp_port());
    let dev_dir = member.dir.join("build/server-dev");
    std::fs::create_dir_all(&dev_dir).expect("dir");
    let toml = regia_net::server_host::config::DEFAULT_SERVER_TOML
        .replace("levels/arena.level.ron", "levels/yard.level.ron");
    std::fs::write(dev_dir.join("server.toml"), toml).expect("server.toml");
    set_status_port(&dev_dir, status_port, game_port);

    let mut dev = Running(
        Command::new(regia_cli())
            .args(["server", "--project", &member.dir.to_string_lossy()])
            .args(["--port", &game_port.to_string(), "--clients", "1"])
            .stdin(Stdio::null())
            .spawn()
            .expect("regia server"),
    );
    let text = wait_health(&mut dev.0, status_port, |t| t.contains("\"players\":1"));
    assert!(text.contains("levels/yard.level.ron"), "{text}");
    let server_bin = dev_dir.join("bin").join(format!("{name}-server"));
    let pid = Command::new("pgrep")
        .args(["-f", &server_bin.to_string_lossy()])
        .output()
        .expect("pgrep");
    let pid = String::from_utf8_lossy(&pid.stdout);
    let pid = pid.lines().next().expect("the server process").trim().to_owned();
    signal(pid.parse().expect("pid"), "TERM");
    let status = dev.0.wait().expect("wait");
    assert!(status.success(), "the dev loop ends with the server: {status}");
}

/// Spec 76 § 10, § 12: `regia server --agent` runs the agent protocol in the headless process, and a
/// SIGTERM to `regia server` drains the server.
#[test]
#[ignore = "builds the server with the agent; invoke with -- --ignored"]
fn the_agent_protocol_runs_in_a_dev_server() {
    let name = format!("server-agent-{}", std::process::id());
    let member = game_copy(&name);
    let (status_port, game_port) = (free_tcp_port(), free_udp_port());
    let dev_dir = member.dir.join("build/server-dev");
    std::fs::create_dir_all(&dev_dir).expect("dir");
    let toml = regia_net::server_host::config::DEFAULT_SERVER_TOML
        .replace("levels/arena.level.ron", "levels/yard.level.ron");
    std::fs::write(dev_dir.join("server.toml"), toml).expect("server.toml");
    set_status_port(&dev_dir, status_port, game_port);

    let mut dev = Running(
        Command::new(regia_cli())
            .args(["server", "--project", &member.dir.to_string_lossy(), "--agent"])
            .stdin(Stdio::null())
            .spawn()
            .expect("regia server"),
    );
    wait_health(&mut dev.0, status_port, |t| t.contains("\"state\":\"running\""));
    let mut answer = None;
    for _ in 0..3_000 {
        let call = Command::new(regia_cli())
            .args([
                "agent",
                "call",
                "server.status",
                "--project",
                &member.dir.to_string_lossy(),
            ])
            .output()
            .expect("agent call");
        if call.status.success() {
            answer = Some(String::from_utf8_lossy(&call.stdout).into_owned());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let answer = answer.expect("the agent answers");
    assert!(answer.contains("levels/yard.level.ron"), "{answer}");
    signal(dev.0.id(), "TERM");
    let status = dev.0.wait().expect("wait");
    assert!(
        status.success(),
        "a SIGTERM drains through `regia server`: {status}"
    );
}
