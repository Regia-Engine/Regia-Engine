// SPDX-License-Identifier: MPL-2.0
use super::*;

/// A scratch project, removed at the end of the test.
struct Project(PathBuf);

impl Project {
    fn new(tag: &str) -> Self {
        let dir = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(dir.join(GENERATORS_DIR)).expect("generators dir");
        Self(dir)
    }

    fn declare(&self, name: &str, toml: &str) {
        std::fs::write(generator_path(&self.0, name), toml).expect("write generator");
    }

    /// Every file of the project outside `.regia/`, with its bytes, sorted by path.
    fn snapshot(&self) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for entry in walkdir::WalkDir::new(&self.0).sort_by_file_name() {
            let entry = entry.expect("walk");
            let rel = entry
                .path()
                .strip_prefix(&self.0)
                .expect("under project")
                .to_string_lossy()
                .replace('\\', "/");
            if !entry.file_type().is_file() || rel.starts_with(".regia") {
                continue;
            }
            out.push((rel, std::fs::read(entry.path()).expect("read")));
        }
        out
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const HEIGHTS: &str = r#"
format_version = 1

[generator]
kind = "heightfield"
seed = 7
description = "test ground"
outputs = ["assets/gen/ground.r16"]

[generator.heightfield]
width = 33
height = 17
octaves = 4
"#;

#[test]
fn a_heightfield_declaration_parses_and_validates() {
    let def = parse_generator("ground", HEIGHTS.as_bytes()).expect("parse");
    assert_eq!(def.kind, GeneratorKind::Heightfield);
    assert_eq!(def.seed, 7);
    let params = def.heightfield.as_ref().expect("params");
    assert_eq!((params.width, params.height, params.octaves), (33, 17, 4));
    assert!((params.frequency - 1.0 / 64.0).abs() < 1e-12, "default frequency");
    assert_eq!(
        def.output_paths(),
        vec![("assets/gen/ground.r16".to_string(), false)]
    );
}

#[test]
fn rules_reject_bad_declarations_with_stable_codes() {
    let cases = [
        (
            HEIGHTS.replace("format_version = 1", "format_version = 2"),
            "gen_unsupported_version",
        ),
        (
            HEIGHTS.replace("format_version = 1", "format_version = 0"),
            "gen_invalid",
        ),
        (
            HEIGHTS.replace("assets/gen/ground.r16", "../outside.r16"),
            "gen_invalid",
        ),
        (
            HEIGHTS.replace("assets/gen/ground.r16", "/abs/ground.r16"),
            "gen_invalid",
        ),
        (
            HEIGHTS.replace("assets/gen/ground.r16", ".regia/ground.r16"),
            "gen_invalid",
        ),
        (
            HEIGHTS.replace("assets/gen/ground.r16", "generators/x.r16"),
            "gen_invalid",
        ),
        (
            HEIGHTS.replace("assets/gen/ground.r16", "Generators/x.r16"),
            "gen_invalid",
        ),
        (
            HEIGHTS.replace("assets/gen/ground.r16", ".REGIA/x.r16"),
            "gen_invalid",
        ),
        (
            HEIGHTS.replace("assets/gen/ground.r16", ".git/x.r16"),
            "gen_invalid",
        ),
        (
            HEIGHTS.replace("assets/gen/ground.r16", "assets/gen/ground.png"),
            "gen_invalid",
        ),
        (HEIGHTS.replace("width = 33", "width = 1"), "gen_invalid"),
        (HEIGHTS.replace("octaves = 4", "octaves = 0"), "gen_invalid"),
        (
            HEIGHTS.replace("seed = 7", "seed = 7\nunknown = 1"),
            "gen_invalid",
        ),
        (
            HEIGHTS.replace("kind = \"heightfield\"", "kind = \"command\""),
            "gen_invalid",
        ),
        (
            HEIGHTS.replace("kind = \"heightfield\"", "kind = \"magic\""),
            "gen_invalid",
        ),
    ];
    for (text, code) in cases {
        let err = parse_generator("ground", text.as_bytes()).expect_err(&text);
        assert_eq!(err.code(), code, "{err}\n{text}");
    }
    let overlapping = r#"
format_version = 1
[generator]
kind = "command"
seed = 1
command = ["true"]
outputs = ["assets/gen/", "assets/gen/a.ron"]
"#;
    let err = parse_generator("x", overlapping.as_bytes()).expect_err("overlap");
    assert!(err.to_string().contains("overlaps"), "{err}");
    let top_level = overlapping.replace(r#"["assets/gen/", "assets/gen/a.ron"]"#, r#"["assets/"]"#);
    let err = parse_generator("x", top_level.as_bytes()).expect_err("top-level folder");
    assert!(err.to_string().contains("top-level folder"), "{err}");
}

#[test]
fn only_generator_tables_report_a_version() {
    assert_eq!(generator_file_version(HEIGHTS.as_bytes()), Ok(Some(1)));
    assert_eq!(generator_file_version(b"[package]\nname = \"x\"\n"), Ok(None));
    assert_eq!(
        generator_file_version(b"format_version = 1\n[server]\nport = 1\n"),
        Ok(None)
    );
    assert!(generator_file_version(b"[generator]\nkind = \"command\"\n").is_err());
    assert!(generator_file_version(b"format_version = \"one\"\n[generator]\n").is_err());
    assert!(generator_file_version(b"not = = toml").is_err());
}

#[test]
fn the_heightfield_is_deterministic_and_follows_the_seed() {
    let def = parse_generator("ground", HEIGHTS.as_bytes()).expect("parse");
    let params = def.heightfield.expect("params");
    let a = heightfield_r16(&params, 7);
    let b = heightfield_r16(&params, 7);
    let c = heightfield_r16(&params, 8);
    assert_eq!(a.len(), 33 * 17 * 2);
    assert_eq!(a, b, "same seed, same bytes");
    assert_ne!(a, c, "another seed, other bytes");
    // Not flat: the noise spans a useful part of the range.
    let samples: Vec<u16> = a
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| u16::from_le_bytes(*p))
        .collect();
    let (min, max) = (samples.iter().min().copied(), samples.iter().max().copied());
    assert!(max.unwrap_or(0) - min.unwrap_or(0) > 1000, "{min:?}..{max:?}");
}

#[test]
fn gen_run_twice_leaves_the_project_byte_identical() {
    let project = Project::new("gen-run-twice");
    project.declare("ground", HEIGHTS);
    let first = run(&project.0, "ground").expect("first run");
    assert_eq!(first.outputs.len(), 1);
    assert!(
        first.outputs[0].written && first.outputs[0].provenance_written,
        "{first:?}"
    );
    let after_first = project.snapshot();
    let second = run(&project.0, "ground").expect("second run");
    assert!(
        !second.outputs[0].written,
        "unchanged bytes are not rewritten: {second:?}"
    );
    assert!(
        !second.outputs[0].provenance_written,
        "the .meta is not rewritten: {second:?}"
    );
    assert_eq!(first.outputs[0].hash, second.outputs[0].hash);
    assert_eq!(
        after_first,
        project.snapshot(),
        "byte-identical project after the second run"
    );
    // The staging directory is gone.
    let staging = project.0.join(STAGING_DIR);
    let empty = match std::fs::read_dir(&staging) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => true,
    };
    assert!(empty, "staging left behind");
}

#[test]
fn gen_run_records_generator_and_seed_in_the_meta() {
    let project = Project::new("gen-provenance");
    project.declare("ground", HEIGHTS);
    run(&project.0, "ground").expect("run");
    let output = project.0.join("assets/gen/ground.r16");
    let meta = regia_asset::MetaSidecar::from_ron(
        &std::fs::read(regia_asset::meta_path_for(&output)).expect("meta"),
    )
    .expect("parse meta");
    let provenance = meta.provenance.expect("provenance");
    assert_eq!(provenance.tool, regia_asset::ProvenanceTool::Generator);
    assert_eq!(provenance.generator.as_deref(), Some("ground"));
    assert_eq!(provenance.seed, Some(7));
    assert_eq!(provenance.origin.as_deref(), Some("generators/ground.toml"));
    assert_eq!(provenance.reviewer, regia_asset::ReviewerState::Pending);
    // A new seed restamps the record.
    project.declare("ground", &HEIGHTS.replace("seed = 7", "seed = 9"));
    let report = run(&project.0, "ground").expect("rerun");
    assert!(report.outputs[0].written && report.outputs[0].provenance_written);
    let meta = regia_asset::MetaSidecar::from_ron(
        &std::fs::read(regia_asset::meta_path_for(&output)).expect("meta"),
    )
    .expect("parse meta");
    assert_eq!(meta.provenance.and_then(|p| p.seed), Some(9));
}

#[test]
fn gen_verify_passes_after_a_run_and_catches_a_hand_edit() {
    let project = Project::new("gen-verify");
    project.declare("ground", HEIGHTS);
    run(&project.0, "ground").expect("run");
    let report = verify(&project.0, &[]).expect("verify");
    assert!(report.ok(), "{report:?}");
    assert_eq!(report.generators.len(), 1);
    assert_eq!(report.generators[0].files, 1);

    // A hand edit of one byte in the middle.
    let output = project.0.join("assets/gen/ground.r16");
    let mut bytes = std::fs::read(&output).expect("read");
    bytes[100] ^= 0x5a;
    std::fs::write(&output, &bytes).expect("edit");
    let report = verify(&project.0, &["ground".to_string()]).expect("verify");
    assert!(!report.ok());
    assert_eq!(report.diffs.len(), 1, "{report:?}");
    let diff = &report.diffs[0];
    assert_eq!(diff.kind, DiffKind::Changed);
    assert_eq!(diff.path, "assets/gen/ground.r16");
    assert_eq!(diff.first_difference, Some(100));
    assert_ne!(diff.expected_hash, diff.found_hash);
    // Verify never writes the project.
    assert_eq!(std::fs::read(&output).expect("read"), bytes);

    // A deleted output is missing.
    std::fs::remove_file(&output).expect("delete");
    let report = verify(&project.0, &[]).expect("verify");
    assert_eq!(report.diffs[0].kind, DiffKind::Missing);
}

#[test]
fn windows_verbatim_paths_lose_their_prefix_for_external_tools() {
    assert_eq!(plain_path(Path::new(r"\\?\C:\work\game")), r"C:\work\game");
    assert_eq!(
        plain_path(Path::new(r"\\?\UNC\server\share")),
        r"\\?\UNC\server\share"
    );
    assert_eq!(plain_path(Path::new("/home/p/game")), "/home/p/game");
}

#[test]
fn verify_of_an_unknown_generator_is_not_found() {
    let project = Project::new("gen-verify-unknown");
    let err = verify(&project.0, &["nope".to_string()]).expect_err("unknown");
    assert_eq!(err.code(), "gen_not_found");
    let err = run(&project.0, "Bad Name").expect_err("bad name");
    assert_eq!(err.code(), "gen_not_found");
    assert!(
        verify(&project.0, &[]).expect("empty").ok(),
        "no generators, nothing differs"
    );
}

#[test]
fn a_missing_input_stops_the_run() {
    let project = Project::new("gen-input");
    project.declare(
        "ground",
        &HEIGHTS.replace("outputs =", "inputs = [\"data/terrain.ron\"]\noutputs ="),
    );
    let err = run(&project.0, "ground").expect_err("input missing");
    assert_eq!(err.code(), "gen_input_missing");
    std::fs::create_dir_all(project.0.join("data")).expect("dir");
    std::fs::write(project.0.join("data/terrain.ron"), "()").expect("input");
    run(&project.0, "ground").expect("runs once the input exists");
}

#[test]
fn list_reports_every_declaration_in_name_order() {
    let project = Project::new("gen-list");
    project.declare("b-ground", HEIGHTS);
    project.declare("a-ground", HEIGHTS);
    project.declare(
        "broken",
        "format_version = 1\n[generator]\nkind = \"heightfield\"\n",
    );
    std::fs::write(project.0.join(GENERATORS_DIR).join("Upper.toml"), HEIGHTS).expect("write");
    let listed = list(&project.0);
    let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["Upper", "a-ground", "b-ground", "broken"]);
    assert!(listed[0].1.is_err() && listed[1].1.is_ok() && listed[2].1.is_ok() && listed[3].1.is_err());
}

#[cfg(unix)]
mod command {
    use super::*;

    /// A command generator: a shell script that writes a folder of files from the seed.
    fn script_generator(project: &Project, name: &str, body: &str) {
        let script = project.0.join("tools").join(format!("{name}.sh"));
        std::fs::create_dir_all(script.parent().expect("parent")).expect("tools");
        std::fs::write(&script, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("script");
        project.declare(
            name,
            &format!(
                r#"
format_version = 1

[generator]
kind = "command"
seed = 42
command = ["sh", "tools/{name}.sh", "{{seed}}"]
inputs = ["tools/{name}.sh"]
outputs = ["assets/gen/{name}/"]
timeout_secs = 60
"#
            ),
        );
    }

    #[test]
    fn a_command_generator_gets_its_seed_and_output_root() {
        let project = Project::new("gen-command");
        script_generator(
            &project,
            "rocks",
            r#"mkdir -p "$REGIA_GEN_OUT/assets/gen/rocks"
for i in 1 2 3; do echo "rock $i seed $1 env $REGIA_GEN_SEED" > "$REGIA_GEN_OUT/assets/gen/rocks/rock_$i.txt"; done"#,
        );
        let report = run(&project.0, "rocks").expect("run");
        let paths: Vec<&str> = report.outputs.iter().map(|o| o.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "assets/gen/rocks/rock_1.txt",
                "assets/gen/rocks/rock_2.txt",
                "assets/gen/rocks/rock_3.txt"
            ]
        );
        let text = std::fs::read_to_string(project.0.join("assets/gen/rocks/rock_2.txt")).expect("read");
        assert_eq!(text, "rock 2 seed 42 env 42\n");
        let before = project.snapshot();
        run(&project.0, "rocks").expect("second run");
        assert_eq!(before, project.snapshot(), "byte-identical");
        assert!(verify(&project.0, &[]).expect("verify").ok());

        // An extra hand-made file in the generator's folder is reported, then removed by a run.
        std::fs::write(project.0.join("assets/gen/rocks/stray.txt"), "hand made").expect("stray");
        let report = verify(&project.0, &[]).expect("verify");
        assert_eq!(report.diffs.len(), 1, "{report:?}");
        assert_eq!(report.diffs[0].kind, DiffKind::Extra);
        assert_eq!(report.diffs[0].path, "assets/gen/rocks/stray.txt");
        let ran = run(&project.0, "rocks").expect("run");
        assert_eq!(ran.removed, ["assets/gen/rocks/stray.txt"]);
        assert!(verify(&project.0, &[]).expect("verify").ok());
    }

    #[test]
    fn a_nondeterministic_generator_fails_verify() {
        let project = Project::new("gen-nondet");
        // The staging directory name differs per run (pid + counter), so this output does too.
        script_generator(
            &project,
            "noisy",
            r#"mkdir -p "$REGIA_GEN_OUT/assets/gen/noisy"
echo "$REGIA_GEN_OUT" > "$REGIA_GEN_OUT/assets/gen/noisy/out.txt""#,
        );
        run(&project.0, "noisy").expect("run");
        let report = verify(&project.0, &[]).expect("verify");
        assert!(!report.ok());
        assert_eq!(report.diffs[0].kind, DiffKind::Changed);
    }

    #[test]
    fn command_failures_have_stable_codes() {
        let project = Project::new("gen-command-fail");
        script_generator(&project, "fails", "echo broken input >&2\nexit 3");
        let err = run(&project.0, "fails").expect_err("fails");
        assert_eq!(err.code(), "gen_failed");
        assert!(err.to_string().contains("broken input"), "{err}");

        script_generator(&project, "lazy", "exit 0");
        let err = run(&project.0, "lazy").expect_err("no output");
        assert_eq!(err.code(), "gen_output_missing");
        assert!(!project.0.join("assets/gen/lazy").exists(), "nothing installed");

        project.declare(
            "absent",
            "format_version = 1\n[generator]\nkind = \"command\"\nseed = 1\ncommand = [\"/nonexistent/regia-gen-tool\"]\noutputs = [\"assets/x.ron\"]\n",
        );
        assert_eq!(
            run(&project.0, "absent").expect_err("spawn").code(),
            "gen_spawn_failed"
        );

        // verify keeps going after a failed generator and reports it.
        let report = verify(&project.0, &[]).expect("verify");
        assert!(!report.ok());
        assert!(report.generators.iter().all(|g| g.error.is_some()), "{report:?}");
    }

    #[test]
    fn a_symlinked_output_folder_is_refused() {
        let project = Project::new("gen-symlink");
        let outside = regia_test_utils::unique_temp_dir("gen-symlink-outside");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("keep.txt"), "not the generator's").expect("keep");
        std::fs::create_dir_all(project.0.join("assets/gen")).expect("assets/gen");
        std::os::unix::fs::symlink(&outside, project.0.join("assets/gen/rocks")).expect("symlink");
        script_generator(
            &project,
            "rocks",
            r#"mkdir -p "$REGIA_GEN_OUT/assets/gen/rocks"
echo rock > "$REGIA_GEN_OUT/assets/gen/rocks/rock.txt""#,
        );
        assert_eq!(
            run(&project.0, "rocks").expect_err("symlink").code(),
            "gen_path_denied"
        );
        let report = verify(&project.0, &[]).expect("verify");
        assert!(
            report.generators[0]
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("gen_path_denied")),
            "{report:?}"
        );
        assert!(
            outside.join("keep.txt").is_file(),
            "nothing outside the project was removed"
        );
        assert!(
            !outside.join("rock.txt").exists(),
            "nothing written outside the project"
        );
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// ADR-2821: a folder output cannot write or remove what a caller denies, and nothing is
    /// installed when one file is denied.
    #[test]
    fn a_denied_path_stops_the_run_before_anything_is_installed() {
        let project = Project::new("gen-deny");
        project.declare(
            "folder",
            r#"format_version = 1
[generator]
kind = "command"
seed = 1
command = ["sh", "-c", "mkdir -p $REGIA_GEN_OUT/assets/gen; echo new > $REGIA_GEN_OUT/assets/gen/a.bin"]
outputs = ["assets/gen/"]
"#,
        );
        // A file the run does not write, in a denied place: the run would remove it.
        let kept = project.0.join("assets/gen/goldens/x.png");
        std::fs::create_dir_all(kept.parent().expect("parent")).expect("dir");
        std::fs::write(&kept, b"image").expect("file");
        let control = GenControl {
            deny_path: Some(Arc::new(|path: &str| path.contains("goldens"))),
            ..GenControl::default()
        };
        let refused = run_observed(&project.0, "folder", &control).expect_err("denied");
        assert_eq!(refused.code(), "gen_path_denied");
        assert_eq!(std::fs::read(&kept).expect("file"), b"image");
        assert!(!project.0.join("assets/gen/a.bin").exists(), "nothing installed");
        // A file the run does write, in a denied place.
        let control = GenControl {
            deny_path: Some(Arc::new(|path: &str| path.ends_with("a.bin"))),
            ..GenControl::default()
        };
        let refused = run_observed(&project.0, "folder", &control).expect_err("denied");
        assert_eq!(refused.code(), "gen_path_denied");
        assert!(!project.0.join("assets/gen/a.bin").exists());
    }

    /// A command that leaves a background process holding stderr: the leader's exit does not end
    /// the wait on that pipe, and the cancel still works (and stops that process: before
    /// fixrev6-gen-cancel-tree this test left `sleep 600` running for ten minutes).
    #[test]
    fn a_process_left_behind_does_not_block_a_cancel() {
        let project = Project::new("gen-left-behind");
        project.declare("bg", &left_behind(0, true));
        // Cancels only once the command has run and left its process behind.
        let control = pid_control(&project, |pids| pids.is_some());
        let refused = run_observed(&project.0, "bg", &control).expect_err("cancelled");
        let bg = reported(&project);
        assert_eq!(refused.code(), "gen_cancelled");
        assert!(!project.0.join("assets/x.ron").exists(), "nothing installed");
        assert!(
            bg.gone(),
            "the background process {} still runs after the cancel",
            bg.0
        );
    }

    /// Runs `ps -o stat= -p <pid>`: the process state letters, or `None` when there is no such
    /// process.
    fn state_of(pid: u32) -> Option<String> {
        let out = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !state.is_empty()).then_some(state)
    }

    /// A process a test watches by pid; killed on drop, so a failing test leaves nothing running.
    struct Watched(u32);

    impl Watched {
        /// Runs and is not a zombie: a killed orphan waits for init (or a subreaper) to reap it,
        /// which a test cannot control.
        fn running(pid: u32) -> bool {
            state_of(pid).is_some_and(|state| !state.starts_with('Z'))
        }

        /// True once the process has stopped running. Polls, bounded (about 10 s), because the
        /// kill and the reaping happen in other processes.
        fn gone(&self) -> bool {
            for _ in 0..200 {
                if !Self::running(self.0) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            false
        }
    }

    impl Drop for Watched {
        fn drop(&mut self) {
            if Self::running(self.0) {
                let _ = Command::new("kill")
                    .args(["-KILL", &self.0.to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }

    /// The file a left-behind command writes `<background pid> <leader pid>` to (atomically,
    /// before it exits), in the project folder (its working directory).
    const PIDS_FILE: &str = "bg.pids";

    /// The pids in [`PIDS_FILE`], once it is there.
    fn pids_of(project: &Path) -> Option<(u32, u32)> {
        let text = std::fs::read_to_string(project.join(PIDS_FILE)).ok()?;
        let mut words = text.split_whitespace().map(str::parse::<u32>);
        match (words.next(), words.next()) {
            (Some(Ok(bg)), Some(Ok(leader))) => Some((bg, leader)),
            _ => None,
        }
    }

    /// A control whose cancel answers `cancel(pids)` with the pids the command wrote so far.
    fn pid_control(
        project: &Project,
        cancel: impl Fn(Option<(u32, u32)>) -> bool + Send + Sync + 'static,
    ) -> GenControl {
        let dir = project.0.clone();
        GenControl {
            cancel: Some(Arc::new(move || cancel(pids_of(&dir)))),
            ..GenControl::default()
        }
    }

    /// Cancels only after the command's leader has exited and been reaped (`kill -0` fails: an
    /// unreaped zombie still answers it), so the cancel always finds the leader gone and only its
    /// background process left. The pids are written before the leader exits, so the first poll
    /// after the reap (at the latest the one after the stderr grace) cancels.
    fn cancel_after_the_leader(project: &Project) -> GenControl {
        pid_control(project, |pids| {
            pids.is_some_and(|(_, leader)| {
                !Command::new("kill")
                    .args(["-0", &leader.to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
            })
        })
    }

    /// The command of the left-behind tests: writes its output (when `output`), starts
    /// `sleep 600` in the background (it inherits stderr), writes both pids to [`PIDS_FILE`] and
    /// exits with `exit`.
    fn left_behind(exit: u8, output: bool) -> String {
        let write = if output {
            "mkdir -p $REGIA_GEN_OUT/assets; echo x > $REGIA_GEN_OUT/assets/x.ron; "
        } else {
            ""
        };
        format!(
            r#"format_version = 1
[generator]
kind = "command"
seed = 1
command = ["sh", "-c", "{write}sleep 600 & echo $! $$ > {PIDS_FILE}.tmp; mv {PIDS_FILE}.tmp {PIDS_FILE}; exit {exit}"]
outputs = ["assets/x.ron"]
"#
        )
    }

    fn reported(project: &Project) -> Watched {
        let (bg, _) = pids_of(&project.0).expect("the command wrote its pids");
        Watched(bg)
    }

    /// fixrev6-gen-cancel-tree: `agent.job_cancel` kills the command's process tree even when the
    /// command itself has already exited and only what it started in the background is left.
    #[test]
    fn a_cancel_after_the_leader_exited_kills_what_it_left_behind() {
        let project = Project::new("gen-cancel-left");
        project.declare("bg", &left_behind(0, true));
        let control = cancel_after_the_leader(&project);
        let refused = run_observed(&project.0, "bg", &control).expect_err("cancelled");
        let bg = reported(&project);
        assert_eq!(refused.code(), "gen_cancelled");
        assert!(!project.0.join("assets/x.ron").exists(), "nothing installed");
        assert!(
            bg.gone(),
            "the background process {} still runs after the cancel",
            bg.0
        );
    }

    /// The same for `gen.verify`: the cancel ends the verify and stops what the generator left.
    #[test]
    fn a_verify_cancel_after_the_leader_exited_kills_what_it_left_behind() {
        let project = Project::new("gen-verify-cancel-left");
        project.declare("bg", &left_behind(0, true));
        let control = cancel_after_the_leader(&project);
        let refused = verify_observed(&project.0, &[], &control).expect_err("cancelled");
        let bg = reported(&project);
        assert_eq!(refused.code(), "gen_cancelled");
        assert!(
            bg.gone(),
            "the background process {} still runs after the cancel",
            bg.0
        );
    }

    /// A cancellable run whose command fails stops what the command left behind.
    #[test]
    fn a_failed_run_kills_what_it_left_behind() {
        let project = Project::new("gen-failed-left");
        project.declare("bg", &left_behind(3, true));
        let control = pid_control(&project, |_| false);
        let failed = run_observed(&project.0, "bg", &control).expect_err("exit 3");
        let bg = reported(&project);
        assert_eq!(failed.code(), "gen_failed");
        assert!(
            bg.gone(),
            "the background process {} still runs after the failure",
            bg.0
        );
    }

    /// A cancellable run whose command succeeds but whose run then fails (here: no output) stops
    /// what the command left behind.
    #[test]
    fn a_run_failing_after_the_command_kills_what_it_left_behind() {
        let project = Project::new("gen-no-output-left");
        project.declare("bg", &left_behind(0, false));
        let control = pid_control(&project, |_| false);
        let failed = run_observed(&project.0, "bg", &control).expect_err("no output");
        let bg = reported(&project);
        assert_eq!(failed.code(), "gen_output_missing");
        assert!(
            bg.gone(),
            "the background process {} still runs after the failure",
            bg.0
        );
    }

    /// The stderr lines arrive as they are written, and a removed variable does not reach the command.
    #[test]
    fn a_run_streams_lines_and_hides_variables() {
        let project = Project::new("gen-observed");
        project.declare(
            "lines",
            r#"format_version = 1
[generator]
kind = "command"
seed = 1
command = ["sh", "-c", "mkdir -p $REGIA_GEN_OUT/assets; echo first >&2; echo home=${HOME:-gone} >&2; echo x > $REGIA_GEN_OUT/assets/x.ron"]
outputs = ["assets/x.ron"]
"#,
        );
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let control = GenControl {
            on_line: Some(Arc::new(move |line: &str| {
                sink.lock().expect("lines").push(line.to_string());
            })),
            env_remove: vec!["HOME".to_string()],
            ..GenControl::default()
        };
        let report = run_observed(&project.0, "lines", &control);
        assert!(report.is_ok(), "{report:?}");
        let seen = lines.lock().expect("lines").clone();
        assert_eq!(seen, ["first", "home=gone"]);
    }

    /// A cancellable run that succeeds and is not cancelled leaves what it started running (a
    /// generator may start a build server on purpose).
    #[test]
    fn a_successful_run_leaves_its_background_process_alone() {
        let project = Project::new("gen-success-left");
        project.declare(
            "bg",
            &format!(
                r#"format_version = 1
[generator]
kind = "command"
seed = 1
command = ["sh", "-c", "touch keep; (while [ -e keep ]; do sleep 0.05; done) & echo $! $$ > {PIDS_FILE}.tmp; mv {PIDS_FILE}.tmp {PIDS_FILE}; mkdir -p $REGIA_GEN_OUT/assets; echo x > $REGIA_GEN_OUT/assets/x.ron"]
outputs = ["assets/x.ron"]
"#
            ),
        );
        let control = pid_control(&project, |_| false);
        let report = run_observed(&project.0, "bg", &control);
        let bg = reported(&project);
        assert!(report.is_ok(), "{report:?}");
        let still_running = Watched::running(bg.0);
        // The loop ends when `keep` goes.
        std::fs::remove_file(project.0.join("keep")).expect("end the background loop");
        assert!(still_running, "a successful run killed its background process");
        assert!(bg.gone(), "the loop ends on its own");
    }

    #[test]
    fn a_hung_command_is_stopped_at_its_timeout() {
        let project = Project::new("gen-timeout");
        project.declare(
            "hang",
            "format_version = 1\n[generator]\nkind = \"command\"\nseed = 1\ncommand = [\"sleep\", \"30\"]\ntimeout_secs = 1\noutputs = [\"assets/x.ron\"]\n",
        );
        assert_eq!(
            run(&project.0, "hang").expect_err("timeout").code(),
            "gen_timeout"
        );
    }

    /// audit F4: only the end of a flood is kept.
    #[test]
    fn stderr_keeps_only_the_tail() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let tail = Mutex::new(Vec::new());
        drain_tail(std::io::Cursor::new(data.clone()), &tail, None);
        let kept = tail.lock().expect("lock").clone();
        assert_eq!(kept.len(), STDERR_TAIL);
        assert_eq!(kept, data[data.len() - STDERR_TAIL..]);
    }

    /// The bounded drain still hands every stderr line to the job's line callback (`gen.run` as a
    /// job), including a last line with no newline; a line longer than the tail comes in pieces.
    #[test]
    fn stderr_lines_reach_the_callback_while_the_tail_stays_bounded() {
        let mut data = b"first\nsecond line\r\n".to_vec();
        data.extend(std::iter::repeat_n(b'x', STDERR_TAIL + 10));
        data.extend_from_slice(b"\nlast");
        let lines = Mutex::new(Vec::<String>::new());
        let on_line: &(dyn Fn(&str) + Send + Sync) = &|line: &str| {
            if let Ok(mut lines) = lines.lock() {
                lines.push(line.to_owned());
            }
        };
        let tail = Mutex::new(Vec::new());
        drain_tail(std::io::Cursor::new(data.clone()), &tail, Some(on_line));
        let lines = lines.lock().expect("lock").clone();
        assert_eq!(lines[0], "first");
        assert_eq!(lines[1], "second line");
        assert_eq!(lines[2].len(), STDERR_TAIL);
        assert_eq!(lines[3], "x".repeat(10));
        assert_eq!(lines.last().map(String::as_str), Some("last"));
        assert_eq!(lines.len(), 5);
        assert_eq!(tail.lock().expect("lock").len(), STDERR_TAIL);
    }

    /// audit F4: a background process that keeps stderr open must not hang a finished command.
    /// Before the fix the join blocked until the process ended, i.e. forever here (`keep` is
    /// removed after `run` returns).
    #[cfg(unix)]
    #[test]
    fn a_background_process_holding_stderr_does_not_hang_the_cook() {
        let project = Project::new("gen-bg-stderr");
        project.declare(
            "bg",
            "format_version = 1\n[generator]\nkind = \"command\"\nseed = 1\ncommand = [\"sh\", \"-c\", \"touch keep; (while [ -e keep ]; do sleep 0.05; done) & echo boom >&2; exit 3\"]\noutputs = [\"assets/x.ron\"]\n",
        );
        let err = run(&project.0, "bg").expect_err("exit 3 fails");
        // The background loop ends when `keep` goes: `Project` removes the folder on drop.
        std::fs::remove_file(project.0.join("keep")).expect("end the background loop");
        assert_eq!(err.code(), "gen_failed");
        assert!(err.to_string().contains("boom"), "{err}");
    }

    /// audit F10: a Windows-authored program path with a backslash starts the same file.
    #[cfg(unix)]
    #[test]
    fn a_backslash_program_path_names_the_project_file() {
        use std::os::unix::fs::PermissionsExt;
        let project = Project::new("gen-backslash-program");
        std::fs::create_dir_all(project.0.join("tools")).expect("tools");
        let script = project.0.join("tools/gen");
        std::fs::write(&script, "#!/bin/sh\nexit 7\n").expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        project.declare(
            "win",
            "format_version = 1\n[generator]\nkind = \"command\"\nseed = 1\ncommand = [\"tools\\\\gen\"]\noutputs = [\"assets/x.ron\"]\n",
        );
        // Found and run (exit 7), not "spawn failed".
        assert_eq!(run(&project.0, "win").expect_err("exit 7").code(), "gen_failed");
    }
}
