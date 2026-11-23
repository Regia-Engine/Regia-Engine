// SPDX-License-Identifier: MPL-2.0
//! `regia install-android`: package, `adb install`, launch, attach the agent protocol through
//! `adb forward` and stream logcat (`docs/specs/21-tooling-and-packaging.md` § 4 Android step 5,
//! `docs/specs/23-agent-native.md` § 2 "Android dev builds").

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;

use regia_agent::protocol::Endpoint;

use super::package::{self, debug_apk_name};
use crate::android::{launch_component, toolchain};
use crate::manifest::Manifest;
use crate::process;

/// How long to wait for the app process to appear after `am start`.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to wait for the app to advertise its agent server (first launch builds pipelines).
const AGENT_TIMEOUT: Duration = Duration::from_secs(60);

/// Arguments of `regia install-android`.
#[derive(Debug, Clone, Args)]
pub struct InstallAndroidArgs {
    /// Project directory (contains `Regia.toml`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// adb serial of the target device (`adb devices`); needed when several are attached.
    #[arg(long)]
    pub device: Option<String>,
    /// Install the APK from the last `regia package` instead of building.
    #[arg(long)]
    pub no_build: bool,
    /// Return after launching instead of streaming the app's logcat.
    #[arg(long)]
    pub no_logcat: bool,
    /// ABI to build (repeatable). Default: the device's primary ABI
    /// (`ro.product.cpu.abi`) when the manifest lists it, otherwise every manifest ABI.
    #[arg(long = "abi")]
    pub abis: Vec<String>,
    /// Only attach the agent protocol to the already running app (`adb forward` + the host-side
    /// `.regia/agent/endpoint.json`); no build, install or launch.
    #[arg(long)]
    pub attach: bool,
}

/// An `adb` invocation addressed to the chosen device.
pub fn adb_command(adb: &Path, device: Option<&str>) -> Command {
    let mut cmd = Command::new(adb);
    if let Some(serial) = device {
        cmd.args(["-s", serial]);
    }
    cmd
}

/// Parses `adb shell pidof` output (`"12345\n"`, possibly several pids) into the first pid.
pub fn parse_pidof(output: &str) -> Option<u32> {
    output.split_whitespace().next()?.parse().ok()
}

/// The ABIs to build for a device whose primary ABI (`getprop ro.product.cpu.abi`) is
/// `device_abi`: that one when the manifest lists it, otherwise every manifest ABI (empty list).
pub fn device_abis(device_abi: Option<&str>, manifest_abis: &[String]) -> Vec<String> {
    match device_abi.map(str::trim) {
        Some(abi) if manifest_abis.iter().any(|listed| listed == abi) => vec![abi.to_owned()],
        _ => Vec::new(),
    }
}

/// Runs `regia install-android`.
pub fn run(args: &InstallAndroidArgs) -> Result<()> {
    let project_dir = args
        .project
        .canonicalize()
        .with_context(|| format!("project directory {} not found", args.project.display()))?;
    let manifest = Manifest::load(&project_dir)?;
    let app_id = &manifest.project.id;

    if args.attach {
        let sdk = toolchain::find_sdk().ok();
        let adb = toolchain::find_adb(sdk.as_deref())?;
        let device = args.device.as_deref();
        let pid = wait_for_pid(&adb, device, app_id)?;
        let endpoint = attach_agent(&adb, device, app_id, pid, &project_dir)?;
        println!("agent attached: {endpoint}");
        return Ok(());
    }

    let apk = if args.no_build {
        let apk = package::default_out_dir(&project_dir, "android").join(debug_apk_name(app_id));
        if !apk.is_file() {
            bail!(
                "{} does not exist; run without --no-build to build it",
                apk.display()
            );
        }
        apk
    } else {
        let abis = if args.abis.is_empty() {
            let sdk = toolchain::find_sdk().ok();
            let device_abi = toolchain::find_adb(sdk.as_deref()).ok().and_then(|adb| {
                process::try_output(adb_command(&adb, args.device.as_deref()).args([
                    "shell",
                    "getprop",
                    "ro.product.cpu.abi",
                ]))
            });
            device_abis(device_abi.as_deref(), &manifest.platforms.android.abis)
        } else {
            args.abis.clone()
        };
        package::package_android_debug_apk(&project_dir, None, &abis)?
    };

    let sdk = toolchain::find_sdk().ok();
    let adb = toolchain::find_adb(sdk.as_deref())?;
    let device = args.device.as_deref();
    let adb_fix = "Install the Android SDK platform-tools";

    process::run(
        adb_command(&adb, device).arg("install").arg("-r").arg(&apk),
        adb_fix,
    )
    .context("adb install failed (is a device attached and authorized? see `adb devices`)")?;
    // GrapheneOS (and other hardened builds) make INTERNET a user toggle that starts off for adb
    // installs; without it the agent server cannot bind its loopback socket. Stock Android grants
    // it at install and refuses `pm grant` for it, so failure is fine.
    let _ = process::try_output(adb_command(&adb, device).args([
        "shell",
        "pm",
        "grant",
        app_id,
        "android.permission.INTERNET",
    ]));
    let component = launch_component(app_id);
    process::run(
        adb_command(&adb, device).args(["shell", "am", "start", "-n", &component]),
        adb_fix,
    )
    .context("launching the app failed")?;

    let pid = wait_for_pid(&adb, device, app_id)?;
    println!("{app_id} running as pid {pid}");
    match attach_agent(&adb, device, app_id, pid, &project_dir) {
        Ok(endpoint) => println!("agent attached: {endpoint}"),
        Err(err) => eprintln!("warning: agent not attached: {err:#}"),
    }
    if args.no_logcat {
        return Ok(());
    }
    // Streams until the user presses Ctrl-C (which also ends adb, in the same process group).
    process::run(
        adb_command(&adb, device).args(["logcat", "--pid", &pid.to_string()]),
        adb_fix,
    )
}

/// Device port of an advertised agent URL (`ws://127.0.0.1:35871` → 35871).
pub fn ws_port(url: &str) -> Option<u16> {
    url.trim_end_matches('/').rsplit(':').next()?.parse().ok()
}

/// The host-side view of a device's agent endpoint: the URL goes through the forwarded
/// `local_port` and the token file is the host copy.
pub fn host_endpoint(device: &Endpoint, local_port: u16, token_file: &Path) -> Endpoint {
    Endpoint {
        url: format!("ws://127.0.0.1:{local_port}"),
        token_file: token_file.display().to_string(),
        ..device.clone()
    }
}

/// Reads a file from the app's data directory (`run-as`; debug builds are debuggable).
fn read_app_file(adb: &Path, device: Option<&str>, app_id: &str, path: &str) -> Option<String> {
    process::try_output(adb_command(adb, device).args(["shell", "run-as", app_id, "cat", path]))
}

/// Waits for the app (process `pid`) to advertise its agent server, forwards a host port to it
/// and writes `<project>/.regia/agent/{endpoint.json,token}` so `regia agent call`,
/// `regia mcp --attach` and `Client::connect_project` reach the device. Returns the host URL.
fn attach_agent(
    adb: &Path,
    device: Option<&str>,
    app_id: &str,
    pid: u32,
    project_dir: &Path,
) -> Result<String> {
    let started = Instant::now();
    let advertised = loop {
        let endpoint = read_app_file(adb, device, app_id, "files/agent/endpoint.json")
            .and_then(|text| serde_json::from_str::<Endpoint>(&text).ok());
        // A previous run's file names another pid; wait for this process to write its own.
        if let Some(endpoint) = endpoint.filter(|e| e.pid == pid) {
            break endpoint;
        }
        if started.elapsed() > AGENT_TIMEOUT {
            bail!(
                "{app_id} did not advertise an agent server within {} s (shipping build without the \
                 `agent` feature, or INTERNET denied: see `adb logcat -s regia_agent`)",
                AGENT_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    let token = read_app_file(adb, device, app_id, "files/agent/token")
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .context("cannot read the app's agent token (is the APK debuggable?)")?;
    let device_port =
        ws_port(&advertised.url).with_context(|| format!("unexpected agent url {}", advertised.url))?;
    let (forwarded, _) = process::output(
        adb_command(adb, device).args(["forward", "tcp:0", &format!("tcp:{device_port}")]),
        "Install the Android SDK platform-tools",
    )?;
    let local_port: u16 = forwarded
        .trim()
        .parse()
        .with_context(|| format!("unexpected `adb forward` output: {forwarded}"))?;
    let dir = regia_agent::endpoint::agent_dir(project_dir);
    let host = host_endpoint(&advertised, local_port, &dir.join("token"));
    regia_agent::endpoint::write(&dir, &host, &token)
        .with_context(|| format!("writing {}", dir.display()))?;
    Ok(host.url)
}

fn wait_for_pid(adb: &Path, device: Option<&str>, app_id: &str) -> Result<u32> {
    let started = Instant::now();
    loop {
        let output = process::try_output(adb_command(adb, device).args(["shell", "pidof", app_id]));
        if let Some(pid) = output.as_deref().and_then(parse_pidof) {
            return Ok(pid);
        }
        if started.elapsed() > START_TIMEOUT {
            bail!(
                "{app_id} did not start within {} s; it probably crashed on launch. \
                 Inspect `adb logcat -d -b crash` and `adb logcat -d -s RustStdoutStderr GameActivity`",
                START_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: InstallAndroidArgs,
    }

    #[test]
    fn args_parse() -> Result<()> {
        let args = Wrapper::try_parse_from(["x"])?.args;
        assert_eq!(args.project, PathBuf::from("."));
        assert_eq!(args.device, None);
        assert!(!args.no_build && !args.no_logcat);
        let args = Wrapper::try_parse_from([
            "x",
            "--project",
            "/p",
            "--device",
            "R5CT",
            "--no-build",
            "--no-logcat",
        ])?
        .args;
        assert_eq!(args.project, PathBuf::from("/p"));
        assert_eq!(args.device.as_deref(), Some("R5CT"));
        assert!(args.no_build && args.no_logcat);
        Ok(())
    }

    #[test]
    fn adb_targets_the_device() {
        let args = |cmd: &Command| -> Vec<String> {
            cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
        };
        let mut cmd = adb_command(Path::new("adb"), Some("emulator-5554"));
        cmd.args(["shell", "pidof", "a.b"]);
        assert_eq!(args(&cmd), ["-s", "emulator-5554", "shell", "pidof", "a.b"]);
        assert!(args(&adb_command(Path::new("adb"), None)).is_empty());
    }

    #[test]
    fn agent_endpoint_goes_through_the_forward() {
        assert_eq!(ws_port("ws://127.0.0.1:35871"), Some(35871));
        assert_eq!(ws_port("ws://127.0.0.1:35871/"), Some(35871));
        assert_eq!(ws_port("ws://127.0.0.1"), None);
        let device = Endpoint {
            url: "ws://127.0.0.1:35871".into(),
            pid: 4242,
            protocol: "0.2".into(),
            token_file: "/data/user/0/a.b/files/agent/token".into(),
            app: "main".into(),
        };
        let host = host_endpoint(&device, 51000, Path::new("/p/.regia/agent/token"));
        assert_eq!(host.url, "ws://127.0.0.1:51000");
        assert_eq!(host.token_file, "/p/.regia/agent/token");
        assert_eq!((host.pid, host.protocol.as_str()), (4242, "0.2"));
    }

    #[test]
    fn device_abi_selection() {
        let listed = ["arm64-v8a".to_owned(), "x86_64".to_owned()];
        assert_eq!(device_abis(Some("arm64-v8a\n"), &listed), ["arm64-v8a"]);
        assert_eq!(device_abis(Some("x86_64"), &listed), ["x86_64"]);
        // Unknown or unreadable device ABI: build everything the manifest lists.
        assert!(device_abis(Some("armeabi-v7a"), &listed).is_empty());
        assert!(device_abis(None, &listed).is_empty());
    }

    #[test]
    fn pidof_parsing() {
        assert_eq!(parse_pidof("12345\n"), Some(12345));
        assert_eq!(parse_pidof("12345 678\n"), Some(12345));
        assert_eq!(parse_pidof(""), None);
        assert_eq!(parse_pidof("pidof: not found"), None);
    }

    #[test]
    fn launch_component_uses_the_generated_activity() {
        assert_eq!(
            launch_component("com.example.hello"),
            "com.example.hello/.RegiaActivity"
        );
    }

    #[test]
    fn no_build_without_apk_fails_clearly() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("regia_cli_test_{}_install", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("Regia.toml"), "[project]\nname = \"x\"\nid = \"a.b\"\n")?;
        let args = InstallAndroidArgs {
            project: dir.clone(),
            device: None,
            no_build: true,
            no_logcat: true,
            abis: Vec::new(),
            attach: false,
        };
        let message = run(&args).err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(message.contains("run without --no-build"), "{message}");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
