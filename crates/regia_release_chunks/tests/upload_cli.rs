// SPDX-License-Identifier: MPL-2.0
//! The hosting-CLI upload backend against a fake CLI (ADR-8846).
//!
//! This test binary has no libtest harness (`harness = false` in `Cargo.toml`): run with the
//! first argument `fake-upload-cli` it IS the fake hosting CLI, so the backend starts a real
//! child process on every platform without a shell or an extra binary. The fake reads the
//! credential from `FAKE_HOST_TOKEN` and echoes it on both streams, so a leak would show.
//! No test reads a clock; the time-limit test uses a one-second limit and asserts the code.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_release_chunks::{
    BackendSpec, ChunkError, CliBackend, UploadCliConfig, UploadPlanOptions, parse_backend, parse_listing,
    redact, resolve_secrets, upload_plan_with, upload_with_backend,
};

const TOKEN_NAME: &str = "FAKE_HOST_TOKEN";
const TOKEN: &str = "s3cr3t-token-value-0042";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("fake-upload-cli") {
        return fake_cli(&args[2..]);
    }
    let tests: &[(&str, fn())] = &[
        (
            "uploads_in_plan_order_and_publishes",
            uploads_in_plan_order_and_publishes,
        ),
        (
            "second_upload_skips_same_size_through_list",
            second_upload_skips_same_size_through_list,
        ),
        (
            "the_secret_never_leaves_redaction",
            the_secret_never_leaves_redaction,
        ),
        (
            "a_config_holding_the_secret_is_refused",
            a_config_holding_the_secret_is_refused,
        ),
        (
            "a_missing_secret_is_upload_cli_env",
            a_missing_secret_is_upload_cli_env,
        ),
        (
            "dry_run_prints_the_plan_and_starts_nothing",
            dry_run_prints_the_plan_and_starts_nothing,
        ),
        (
            "a_flaky_put_retries_and_a_dead_one_fails",
            a_flaky_put_retries_and_a_dead_one_fails,
        ),
        (
            "arguments_reach_the_child_without_a_shell",
            arguments_reach_the_child_without_a_shell,
        ),
        (
            "bad_configs_are_upload_cli_config",
            bad_configs_are_upload_cli_config,
        ),
        (
            "a_missing_program_is_upload_cli_spawn",
            a_missing_program_is_upload_cli_spawn,
        ),
        (
            "a_hung_cli_is_stopped_at_the_time_limit",
            a_hung_cli_is_stopped_at_the_time_limit,
        ),
        ("backend_specs_parse", backend_specs_parse),
        ("listing_lines_parse", listing_lines_parse),
    ];
    let filter = args.iter().skip(1).find(|arg| !arg.starts_with('-'));
    let mut failed = 0;
    let mut ran = 0;
    for (name, test) in tests {
        if filter.is_some_and(|f| !name.contains(f.as_str())) {
            continue;
        }
        ran += 1;
        match std::panic::catch_unwind(test) {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                failed += 1;
                println!("test {name} ... FAILED");
            }
        }
    }
    println!(
        "test result: {}. {} passed; {failed} failed",
        if failed == 0 { "ok" } else { "FAILED" },
        ran - failed
    );
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The fake hosting CLI. Modes: `put FILE STORE NAME`, `list STORE`, `publish STORE`,
/// `flaky COUNTER FAILURES FILE STORE NAME`, `fail`, `args OUT REST...`, `hang`.
fn fake_cli(args: &[String]) -> ExitCode {
    let token = std::env::var(TOKEN_NAME).unwrap_or_default();
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or_default();
    match arg(0) {
        "put" => fake_put(Path::new(arg(1)), Path::new(arg(2)), arg(3), &token),
        "list" => {
            let mut names: Vec<_> = fs::read_dir(arg(1))
                .map(|dir| dir.flatten().map(|e| e.path()).collect())
                .unwrap_or_default();
            names.sort();
            for path in names {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                if name == "calls.log" || name == "PUBLISHED" {
                    continue;
                }
                let size = fs::metadata(&path).map(|m| m.len()).unwrap_or_default();
                println!("{size} {name}");
            }
            ExitCode::SUCCESS
        }
        "publish" => {
            append(Path::new(arg(1)), "publish");
            let _ = fs::write(Path::new(arg(1)).join("PUBLISHED"), b"OK\n");
            ExitCode::SUCCESS
        }
        "flaky" => {
            let counter = Path::new(arg(1));
            let seen: u32 = fs::read_to_string(counter)
                .ok()
                .and_then(|t| t.trim().parse().ok())
                .unwrap_or(0)
                + 1;
            let _ = fs::write(counter, seen.to_string());
            let failures: u32 = arg(2).parse().unwrap_or(0);
            if seen <= failures {
                eprintln!("transient failure {seen} for token {token}");
                return ExitCode::from(7);
            }
            fake_put(Path::new(arg(3)), Path::new(arg(4)), arg(5), &token)
        }
        "fail" => {
            eprintln!("access denied for {token}");
            ExitCode::from(3)
        }
        "args" => {
            let _ = fs::write(arg(1), args[2..].join("\n"));
            ExitCode::SUCCESS
        }
        "hang" => loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        },
        other => {
            eprintln!("fake-upload-cli: unknown mode {other:?}");
            ExitCode::from(64)
        }
    }
}

fn fake_put(file: &Path, store: &Path, name: &str, token: &str) -> ExitCode {
    if fs::copy(file, store.join(name)).is_err() {
        eprintln!("cannot copy {}", file.display());
        return ExitCode::from(5);
    }
    append(store, &format!("put {name}"));
    println!("uploaded {name} with token={token}");
    eprintln!("auth header Bearer {token}");
    ExitCode::SUCCESS
}

fn append(store: &Path, line: &str) {
    let path = store.join("calls.log");
    let mut text = fs::read_to_string(&path).unwrap_or_default();
    text.push_str(line);
    text.push('\n');
    let _ = fs::write(path, text);
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "regia-upload-cli-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("temp");
        Self(path)
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn release_dir() -> Scratch {
    let src = Scratch::new("src");
    fs::write(src.join("core.tar.zst"), b"chunk-bytes").expect("chunk");
    fs::write(src.join("readme.txt"), b"payload").expect("payload");
    fs::write(src.join("manifest-demo.json"), b"{\"format\":1}\n").expect("manifest");
    src
}

fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn me() -> String {
    std::env::current_exe()
        .expect("test exe")
        .to_string_lossy()
        .into_owned()
}

/// A config whose program is this test binary in fake-CLI mode.
fn config_text(store: &Path, put: &str, extra: &str) -> String {
    format!(
        "upload_cli = 1\nprogram = {program}\nput = {put}\nlist = [\"fake-upload-cli\", \"list\", \"{{store}}\"]\n\
         publish = [\"fake-upload-cli\", \"publish\", \"{{store}}\"]\nsecret_env = [\"{TOKEN_NAME}\"]\n{extra}\n\
         [vars]\nstore = {store}\n",
        program = quoted(&me()),
        store = quoted(&store.to_string_lossy()),
    )
}

const PUT: &str = r#"["fake-upload-cli", "put", "{file}", "{store}", "{name}"]"#;

fn secrets() -> Vec<(String, String)> {
    vec![(TOKEN_NAME.to_string(), TOKEN.to_string())]
}

fn backend(config: &str, dir: &Path) -> CliBackend {
    let config = UploadCliConfig::parse(config).expect("config");
    CliBackend::new(config, dir, secrets()).expect("backend")
}

fn calls_log(store: &Path) -> Vec<String> {
    fs::read_to_string(store.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn uploads_in_plan_order_and_publishes() {
    let src = release_dir();
    let store = Scratch::new("store");
    let mut cli = backend(&config_text(&store, PUT, ""), &src);
    let receipt = upload_with_backend(&src, &mut cli, &UploadPlanOptions::default()).expect("upload");
    assert_eq!(receipt.puts, ["core.tar.zst", "readme.txt", "manifest-demo.json"]);
    assert!(receipt.published);
    assert_eq!(
        calls_log(&store),
        [
            "put core.tar.zst",
            "put readme.txt",
            "put manifest-demo.json",
            "publish"
        ]
    );
    assert_eq!(
        fs::read(store.join("core.tar.zst")).expect("chunk"),
        b"chunk-bytes"
    );
    assert_eq!(fs::read(store.join("PUBLISHED")).expect("published"), b"OK\n");
    let calls = cli.calls();
    assert_eq!(calls.len(), 5, "list, three puts, publish: {calls:#?}");
    assert_eq!(
        calls[0].argv[1..],
        ["fake-upload-cli", "list", &*store.to_string_lossy()]
    );
    assert_eq!(calls[1].argv[3], src.join("core.tar.zst").to_string_lossy());
    assert!(
        calls
            .iter()
            .all(|call| call.status == Some(0) && call.attempt == 1)
    );
}

fn second_upload_skips_same_size_through_list() {
    let src = release_dir();
    let store = Scratch::new("store-again");
    let mut cli = backend(&config_text(&store, PUT, ""), &src);
    upload_with_backend(&src, &mut cli, &UploadPlanOptions::default()).expect("first");
    fs::write(src.join("readme.txt"), b"payload, longer now").expect("grow");
    let again = upload_with_backend(&src, &mut cli, &UploadPlanOptions::default()).expect("again");
    assert_eq!(again.puts, ["readme.txt"], "only the file whose size changed");
    assert_eq!(again.skipped, ["core.tar.zst", "manifest-demo.json"]);
    assert_eq!(calls_log(&store).last().map(String::as_str), Some("publish"));
}

fn the_secret_never_leaves_redaction() {
    let src = release_dir();
    let store = Scratch::new("store-redact");
    let mut cli = backend(&config_text(&store, PUT, ""), &src);
    upload_with_backend(&src, &mut cli, &UploadPlanOptions::default()).expect("upload");
    let put = &cli.calls()[1];
    assert!(put.stdout.contains("token=<redacted:FAKE_HOST_TOKEN>"), "{put:?}");
    assert!(
        put.stderr.contains("Bearer <redacted:FAKE_HOST_TOKEN>"),
        "{put:?}"
    );
    let all = format!("{:?}", cli.calls());
    assert!(!all.contains(TOKEN), "a call record holds the credential");

    // A fresh store: the first one already holds every file at its size, so list would skip them all.
    let fail_store = Scratch::new("store-redact-fail");
    let failing = config_text(&fail_store, r#"["fake-upload-cli", "fail", "{file}"]"#, "");
    let mut cli = backend(&failing, &src);
    let err = upload_with_backend(&src, &mut cli, &UploadPlanOptions::default()).expect_err("fails");
    assert_eq!(err.code, "upload_cli_failed");
    assert!(err.message.contains("status 3"), "{err}");
    assert!(
        err.message
            .contains("access denied for <redacted:FAKE_HOST_TOKEN>"),
        "{err}"
    );
    assert!(!err.message.contains(TOKEN), "the error holds the credential");
    assert_eq!(
        redact("a s3cr3t-token-value-0042 b", &secrets()),
        "a <redacted:FAKE_HOST_TOKEN> b"
    );
}

fn a_config_holding_the_secret_is_refused() {
    let src = release_dir();
    let store = Scratch::new("store-literal");
    let put =
        format!(r#"["fake-upload-cli", "put", "{{file}}", "{{store}}", "{{name}}", "--token={TOKEN}"]"#);
    let config = UploadCliConfig::parse(&config_text(&store, &put, "")).expect("parses");
    let err = CliBackend::new(config, &src, secrets()).expect_err("refused");
    assert_eq!(err.code, "upload_cli_secret");
    assert!(
        err.message.contains("FAKE_HOST_TOKEN") && err.message.contains("put"),
        "{err}"
    );
    assert!(!err.message.contains(TOKEN));
    assert!(calls_log(&store).is_empty(), "nothing ran");

    let config = UploadCliConfig::parse(&config_text(&store, PUT, ""))
        .expect("parses")
        .with_vars(&[("store".to_string(), format!("/x/{TOKEN}"))])
        .expect("override");
    let err = CliBackend::new(config, &src, secrets()).expect_err("a var holding it too");
    assert_eq!(err.code, "upload_cli_secret");
    assert!(err.message.contains("vars.store"), "{err}");
}

fn a_missing_secret_is_upload_cli_env() {
    let store = Scratch::new("store-env");
    let config = UploadCliConfig::parse(&config_text(&store, PUT, "")).expect("parses");
    let err = resolve_secrets(&config, &|_| None).expect_err("unset");
    assert_eq!(err.code, "upload_cli_env");
    assert!(err.message.contains(TOKEN_NAME));
    let err = resolve_secrets(&config, &|_| Some(String::new())).expect_err("empty");
    assert_eq!(err.code, "upload_cli_env");
    let found =
        resolve_secrets(&config, &|name| (name == TOKEN_NAME).then(|| TOKEN.to_string())).expect("set");
    assert_eq!(found, secrets());
}

fn dry_run_prints_the_plan_and_starts_nothing() {
    let src = release_dir();
    let store = Scratch::new("store-dry");
    let config = UploadCliConfig::parse(&config_text(&store, PUT, "")).expect("parses");
    let opts = UploadPlanOptions {
        dry_run: true,
        ..UploadPlanOptions::default()
    };
    let plan = upload_plan_with(&src, &opts).expect("plan");
    let names: Vec<String> = plan.groups.iter().flat_map(|g| g.files.clone()).collect();
    let commands = config.plan_commands(&src, &names).expect("commands");
    let store_text = store.to_string_lossy().into_owned();
    let words: Vec<Vec<&str>> = commands
        .iter()
        .map(|c| c[1..].iter().map(String::as_str).collect())
        .collect();
    let file = |n: &str| src.join(n).to_string_lossy().into_owned();
    let (core, readme, manifest) = (
        file("core.tar.zst"),
        file("readme.txt"),
        file("manifest-demo.json"),
    );
    assert_eq!(
        words,
        vec![
            vec!["fake-upload-cli", "list", store_text.as_str()],
            vec![
                "fake-upload-cli",
                "put",
                core.as_str(),
                store_text.as_str(),
                "core.tar.zst"
            ],
            vec![
                "fake-upload-cli",
                "put",
                readme.as_str(),
                store_text.as_str(),
                "readme.txt"
            ],
            vec![
                "fake-upload-cli",
                "put",
                manifest.as_str(),
                store_text.as_str(),
                "manifest-demo.json"
            ],
            vec!["fake-upload-cli", "publish", store_text.as_str()],
        ]
    );
    let mut cli = CliBackend::new(config, &src, secrets()).expect("backend");
    let receipt = upload_with_backend(&src, &mut cli, &opts).expect("dry run");
    assert!(receipt.puts.is_empty() && !receipt.published);
    assert!(cli.calls().is_empty(), "a dry run starts no child");
    assert!(calls_log(&store).is_empty());
}

fn a_flaky_put_retries_and_a_dead_one_fails() {
    let src = release_dir();
    let store = Scratch::new("store-flaky");
    let counters = Scratch::new("counters");
    let counter = quoted(&counters.join("n").to_string_lossy());
    let put = format!(r#"["fake-upload-cli", "flaky", {counter}, "2", "{{file}}", "{{store}}", "{{name}}"]"#);
    let config = UploadCliConfig::parse(&config_text(&store, &put, "")).expect("parses");
    let mut cli = CliBackend::new(config, &src, secrets())
        .expect("backend")
        .with_attempts(3);
    let receipt =
        upload_with_backend(&src, &mut cli, &UploadPlanOptions::default()).expect("third attempt wins");
    assert_eq!(receipt.puts.len(), 3);
    let first: Vec<(Option<i32>, u32)> = cli.calls()[1..4].iter().map(|c| (c.status, c.attempt)).collect();
    assert_eq!(first, [(Some(7), 1), (Some(7), 2), (Some(0), 3)]);

    fs::remove_file(counters.join("n")).expect("reset");
    let store2 = Scratch::new("store-dead");
    let config = UploadCliConfig::parse(&config_text(&store2, &put, "")).expect("parses");
    let mut cli = CliBackend::new(config, &src, secrets())
        .expect("backend")
        .with_attempts(2);
    let err = upload_with_backend(&src, &mut cli, &UploadPlanOptions::default())
        .expect_err("two attempts are not enough");
    assert_eq!(err.code, "upload_cli_failed");
    assert!(err.message.contains("after 2 attempt(s)"), "{err}");
    assert!(
        !calls_log(&store2).contains(&"publish".to_string()),
        "a failed put never publishes"
    );
}

fn arguments_reach_the_child_without_a_shell() {
    let src = release_dir();
    let store = Scratch::new("store-args");
    let out = store.join("args.txt");
    let put = format!(
        r#"["fake-upload-cli", "args", {out}, "{{file}}", "{{label}}", "a;b", "$(echo no)", "*"]"#,
        out = quoted(&out.to_string_lossy())
    );
    let text =
        config_text(&store, &put, "").replace("[vars]\n", "[vars]\nlabel = \"two words & 'quotes'\"\n");
    let config = UploadCliConfig::parse(&text).expect("parses with label");
    let mut cli = CliBackend::new(config, &src, secrets()).expect("backend");
    let opts = UploadPlanOptions {
        skip_same_size: false,
        ..UploadPlanOptions::default()
    };
    upload_with_backend(&src, &mut cli, &opts).expect("upload");
    let seen = fs::read_to_string(&out).expect("args file");
    let lines: Vec<&str> = seen.lines().collect();
    let manifest = src.join("manifest-demo.json").to_string_lossy().into_owned();
    assert_eq!(
        lines,
        [
            manifest.as_str(),
            "two words & 'quotes'",
            "a;b",
            "$(echo no)",
            "*"
        ]
    );
}

fn expect_config_error(text: &str, needle: &str) {
    let err = UploadCliConfig::parse(text).expect_err(needle);
    assert_eq!(err.code, "upload_cli_config", "{err}");
    assert!(err.message.contains(needle), "{needle}: {err}");
}

fn bad_configs_are_upload_cli_config() {
    let base = "upload_cli = 1\nprogram = \"host\"\nput = [\"push\", \"{file}\"]\n";
    assert!(UploadCliConfig::parse(base).is_ok());
    expect_config_error(&base.replace("upload_cli = 1", "upload_cli = 2"), "upload_cli 2");
    expect_config_error(&format!("{base}token = \"x\"\n"), "unknown field");
    expect_config_error(&base.replace("\"{file}\"", "\"x\""), "put must pass {file}");
    expect_config_error(
        &base.replace("{file}", "{file} {channel}"),
        "vars does not define",
    );
    expect_config_error(
        &format!("{base}list = [\"ls\", \"{{name}}\"]\n"),
        "list may not use {name}",
    );
    expect_config_error(
        &format!("{base}publish = [\"pub\", \"{{file}}\"]\n"),
        "publish may not use {file}",
    );
    expect_config_error(&format!("{base}[vars]\nFile = \"x\"\n"), "vars key");
    expect_config_error(&format!("{base}[vars]\nname = \"x\"\n"), "vars key");
    expect_config_error(&format!("{base}secret_env = [\"lower\"]\n"), "secret_env entry");
    expect_config_error(&format!("{base}timeout_s = 0\n"), "timeout_s 0");
    expect_config_error(
        &base.replace("\"host\"", "\"{x}\""),
        "program takes no placeholders",
    );
    expect_config_error(&base.replace("{file}", "{file"), "without");
    let err = UploadCliConfig::parse(base)
        .expect("parses")
        .with_vars(&[("nope".to_string(), "x".to_string())])
        .expect_err("undefined var");
    assert_eq!(err.code, "upload_cli_config");
}

fn a_missing_program_is_upload_cli_spawn() {
    let src = release_dir();
    let missing = src.join("no-such-hosting-cli");
    let text = format!(
        "upload_cli = 1\nprogram = {}\nput = [\"push\", \"{{file}}\"]\n",
        quoted(&missing.to_string_lossy())
    );
    let mut cli = backend(&text, &src);
    let err = upload_with_backend(&src, &mut cli, &UploadPlanOptions::default()).expect_err("no program");
    assert_eq!(err.code, "upload_cli_spawn", "{err}");
}

fn a_hung_cli_is_stopped_at_the_time_limit() {
    let src = release_dir();
    let store = Scratch::new("store-hang");
    let config = config_text(
        &store,
        r#"["fake-upload-cli", "hang", "{file}"]"#,
        "timeout_s = 1",
    )
    .replace("list = [\"fake-upload-cli\", \"list\", \"{store}\"]\n", "");
    let mut cli = backend(&config, &src);
    let err = upload_with_backend(&src, &mut cli, &UploadPlanOptions::default()).expect_err("stopped");
    assert_eq!(err.code, "upload_cli_timeout", "{err}");
    assert_eq!(cli.calls().len(), 1, "a timed-out put is not retried");
    assert_eq!(cli.calls()[0].status, None);
}

fn backend_specs_parse() {
    assert_eq!(
        parse_backend("fs:/tmp/x").expect("fs"),
        BackendSpec::Fs(PathBuf::from("/tmp/x"))
    );
    assert_eq!(
        parse_backend("cli:host.toml").expect("cli"),
        BackendSpec::Cli(PathBuf::from("host.toml"))
    );
    let code = |text: &str| parse_backend(text).map_err(|e: ChunkError| e.code).err();
    assert_eq!(code("cli:"), Some("upload_backend"));
    assert_eq!(code("fs:"), Some("upload_backend"));
    assert_eq!(code("https://example.invalid/x"), Some("upload_backend"));
}

fn listing_lines_parse() {
    let files = parse_listing("12 b.bin\n\n3 a name with spaces.txt\n").expect("parses");
    assert_eq!(files[0].name, "a name with spaces.txt");
    assert_eq!(files[0].size, 3);
    assert_eq!(files[1].size, 12);
    let err = parse_listing("12 ok\nnot-a-size x\n").expect_err("bad line");
    assert_eq!(err.code, "upload_cli_list");
    assert!(err.message.contains("line 2"));
}
