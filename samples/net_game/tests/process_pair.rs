// SPDX-License-Identifier: MIT-0
//! The template's real binaries as a process pair (gap audit F3, ADR-2039): `net_game_server` in one
//! process, `net_game_client --headless --clients 2 --bot` in another. They play a minute over
//! encrypted UDP on 127.0.0.1, the key pinned from the server's first line, the `home` conditioner
//! on the server and stated conditions (`--conditioner delay=20,jitter=5,loss=0.01`, ADR-3075) on
//! the clients, and what the processes print must agree. Every process prints its conditions and a
//! `netstats` line, which `cargo xtask net-match` reads across machines.
//!
//! Each binary paces itself at 60 Hz, so this takes a real minute. Nothing here measures time: the
//! test waits for the client process to exit, then closes the server's stdin.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStdout, Command, Stdio};

use net_game::PREDICTION_TOLERANCE_M;

/// The scripted minute.
const MINUTE: u32 = 3600;
/// Ticks the clients keep running after their bots stop, so the last shots and moves settle.
const SETTLE: u32 = 300;
/// The clients' stated conditions.
const CLIENT_CONDITIONS: &str = "delay=20,jitter=5,loss=0.01";

/// The binaries print positions to the millimeter.
const PRINT_ROUNDING_M: f32 = 0.001;

/// Kills the child if the test panics before it exits.
struct Reap(Child);

impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One `key=value` line of a binary's output, after its leading word.
fn fields(line: &str) -> BTreeMap<&str, &str> {
    line.split_whitespace()
        .filter_map(|word| word.split_once('='))
        .collect()
}

#[derive(Debug, PartialEq)]
struct Scores {
    tags: u32,
    tagged: u32,
}

#[derive(Debug)]
struct Seen {
    x: f32,
    z: f32,
    scores: Scores,
}

/// The avatars in the lines starting with `word`, by slot, for `client` when one is named.
fn avatars(output: &str, word: &str, client: Option<&str>) -> BTreeMap<u32, Seen> {
    let mut out = BTreeMap::new();
    for line in output.lines().filter(|l| l.starts_with(word)) {
        let f = fields(line);
        if client.is_some_and(|c| f.get("client") != Some(&c)) {
            continue;
        }
        let number = |name: &str| -> f32 {
            let text = f.get(name).unwrap_or_else(|| panic!("{name} in `{line}`"));
            text.parse().unwrap_or_else(|e| panic!("{name} in `{line}`: {e}"))
        };
        let whole = |name: &str| -> u32 {
            let text = f.get(name).unwrap_or_else(|| panic!("{name} in `{line}`"));
            text.parse().unwrap_or_else(|e| panic!("{name} in `{line}`: {e}"))
        };
        out.insert(
            whole("slot"),
            Seen {
                x: number("x"),
                z: number("z"),
                scores: Scores {
                    tags: whole("tags"),
                    tagged: whole("tagged"),
                },
            },
        );
    }
    out
}

fn read_rest(stdout: &mut BufReader<ChildStdout>) -> String {
    let mut text = String::new();
    stdout.read_to_string(&mut text).expect("server stdout is text");
    text
}

#[test]
fn a_server_process_and_a_client_process_play_a_minute_and_agree() {
    let mut server = Reap(
        Command::new(env!("CARGO_BIN_EXE_net_game_server"))
            .args([
                "--bind",
                "127.0.0.1:0",
                "--until-stdin-eof",
                "--conditioner",
                "home",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the server binary starts"),
    );
    let server_stdin = server.0.stdin.take().expect("piped stdin");
    let mut server_out = BufReader::new(server.0.stdout.take().expect("piped stdout"));

    let mut first = String::new();
    server_out
        .read_line(&mut first)
        .expect("the server prints a line");
    let words: Vec<&str> = first.split_whitespace().collect();
    let [_, addr, _, key] = words[..] else {
        panic!("not `listening <addr> key <key>`: `{first}`");
    };
    assert!(first.starts_with("listening "), "{first}");
    assert_ne!(key, "none", "the server has a key to pin: {first}");

    let client = Command::new(env!("CARGO_BIN_EXE_net_game_client"))
        .args([
            "--headless",
            "--clients",
            "2",
            "--bot",
            "--connect",
            addr,
            "--key",
            key,
        ])
        .args(["--bot-for", &MINUTE.to_string()])
        .args(["--conditioner", CLIENT_CONDITIONS, "--seed", "5"])
        .args(["--ticks", &(MINUTE + SETTLE).to_string()])
        .stdout(Stdio::piped())
        .spawn()
        .expect("the client binary starts");
    let client = client.wait_with_output().expect("the client process ends");
    let client_text = String::from_utf8_lossy(&client.stdout).into_owned();
    assert!(
        client.status.success(),
        "client: {:?}\n{client_text}",
        client.status
    );

    // The last client is done: stop the server, inside its silence timeout, and read its summary.
    drop(server_stdin);
    let server_text = read_rest(&mut server_out);
    let status = server.0.wait().expect("the server process ends");
    assert!(status.success(), "server: {status:?}\n{server_text}");
    println!("{first}{server_text}{client_text}");

    let truth = avatars(&server_text, "summary", None);
    assert_eq!(truth.len(), 2, "two avatars on the server:\n{server_text}");
    let total_tags: u32 = truth.values().map(|a| a.scores.tags).sum();
    assert!(total_tags > 0, "the bots tagged each other:\n{server_text}");

    // Each process states the conditions it runs and reports its counters (ADR-3075).
    assert!(
        server_text
            .lines()
            .any(|l| l
                == "conditioner delay=30 jitter=0 loss=0.005 dup=0.000 reorder=0.000 reorder_max=0 seed=1"),
        "the server states `home`:\n{server_text}"
    );
    assert!(
        client_text
            .lines()
            .any(|l| l
                == "conditioner delay=20 jitter=5 loss=0.010 dup=0.000 reorder=0.000 reorder_max=0 seed=5"),
        "the client states its conditions:\n{client_text}"
    );
    let count = |f: &BTreeMap<&str, &str>, key: &str| -> u64 {
        f.get(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{key} in {f:?}"))
    };
    let server_stats = server_text
        .lines()
        .find(|l| l.starts_with("netstats who=server "))
        .map(fields)
        .unwrap_or_else(|| panic!("a server netstats line:\n{server_text}"));
    assert_eq!(server_stats.get("role"), Some(&"server"));
    assert!(count(&server_stats, "cond_delayed") > 0, "{server_stats:?}");
    assert!(count(&server_stats, "datagrams_in") > 0, "{server_stats:?}");
    assert_eq!(
        count(&server_stats, "hits"),
        u64::from(total_tags),
        "{server_stats:?}"
    );
    assert!(
        count(&server_stats, "shots") >= count(&server_stats, "hits"),
        "{server_stats:?}"
    );
    assert!(
        count(&server_stats, "rewound") > 0,
        "the process judged shots in history: {server_stats:?}"
    );
    // `--clients 2` gives each client its own seed: the given one plus its index.
    for (client, seed) in [("client0", 5), ("client1", 6)] {
        let stats = client_text
            .lines()
            .find(|l| l.starts_with(&format!("netstats who={client} ")))
            .map(fields)
            .unwrap_or_else(|| panic!("a netstats line for {client}:\n{client_text}"));
        assert_eq!(stats.get("role"), Some(&"client"), "{stats:?}");
        // 20 ms on every outbound datagram: every one is held, so the counter is the datagram count.
        assert!(count(&stats, "cond_delayed") > 0, "{stats:?}");
        assert!(
            count(&stats, "bytes_sent") > 0 && count(&stats, "bytes_received") > 0,
            "{stats:?}"
        );
        assert_eq!(count(&stats, "verify_failures"), 0, "{stats:?}");
        let cond = format!("delay=20,jitter=5,loss=0.010,dup=0.000,reorder=0.000,reorder_max=0,seed={seed}");
        assert_eq!(stats.get("cond"), Some(&cond.as_str()), "{stats:?}");
    }

    let tolerance = PREDICTION_TOLERANCE_M + PRINT_ROUNDING_M + 1e-4;
    for client in ["0", "1"] {
        let link = format!("client {client} connected=true sealed=true");
        assert!(
            client_text.lines().any(|l| l == link),
            "`{link}` in:\n{client_text}"
        );

        let seen = avatars(&client_text, "seen", Some(client));
        assert_eq!(
            seen.len(),
            truth.len(),
            "client {client} sees both:\n{client_text}"
        );
        for (slot, want) in &truth {
            let got = &seen[slot];
            assert_eq!(got.scores, want.scores, "client {client}, slot {slot}");
            assert!(
                (got.x - want.x).abs() <= tolerance && (got.z - want.z).abs() <= tolerance,
                "client {client}, slot {slot}: {got:?} against the server's {want:?}"
            );
        }

        let prediction = client_text
            .lines()
            .find(|l| l.starts_with("prediction") && fields(l).get("client") == Some(&client))
            .unwrap_or_else(|| panic!("a prediction line for client {client}:\n{client_text}"));
        assert_eq!(
            fields(prediction).get("verify_failures"),
            Some(&"0"),
            "{prediction}"
        );
    }
}
