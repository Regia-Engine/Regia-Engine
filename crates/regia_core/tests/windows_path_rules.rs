// SPDX-License-Identifier: MPL-2.0
//! The Windows path rules as a table, checked as pure logic so a Linux gate runs them (ADR-3206).
//!
//! A level written on Linux gets opened on Windows, so a project path has to mean the same thing on
//! both. Every row goes through all four entry points of `regia_core::project_path`, because a
//! caller reaches the rule through whichever one fits its file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use regia_core::project_path::{
    ProjectPathError as E, check_relative, is_remote_form, resolve_in_roots, resolve_read, resolve_write,
};

fn temp_root(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-win-path-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp root");
    dir
}

fn device(s: &str) -> E {
    E::Device(s.to_string())
}

/// The refused rows: the input and the rule it breaks.
fn refused() -> Vec<(&'static str, E)> {
    vec![
        // Drive letters.
        ("C:\\x", E::Prefix),
        ("C:/x", E::Prefix),
        ("C:x", E::Prefix),
        ("c:", E::Prefix),
        ("a/C:\\x", E::Prefix),
        // UNC, verbatim and device namespaces.
        ("\\\\server\\share", E::Absolute),
        ("\\\\server\\share\\x", E::Absolute),
        ("//server/share/x", E::Absolute),
        ("\\\\?\\C:\\x", E::Absolute),
        ("\\\\?\\UNC\\server\\share\\x", E::Absolute),
        ("\\\\.\\C:\\x", E::Absolute),
        ("\\\\.\\pipe\\x", E::Absolute),
        // Root-relative.
        ("\\x", E::Absolute),
        ("/x", E::Absolute),
        // Parent segments, with each separator and with both.
        ("..", E::ParentDir),
        ("..\\x", E::ParentDir),
        ("../x", E::ParentDir),
        ("a\\..\\..\\b", E::ParentDir),
        ("a/../../b", E::ParentDir),
        ("a/..\\..\\b", E::ParentDir),
        ("a\\../b", E::ParentDir),
        // Only dots and spaces: no Windows file can be named so, and they look like the parent.
        (".. \\x", E::ParentDir),
        ("... \\x", E::ParentDir),
        ("a\\. .\\b", E::ParentDir),
        ("a/ /b", E::ParentDir),
        // Trailing dots and spaces: Windows opens the last segment without them, and any segment
        // without one dot (see `windows_opens_these_names_as_the_rule_says`).
        ("a.", E::TrailingDotOrSpace),
        ("a ", E::TrailingDotOrSpace),
        ("a. ", E::TrailingDotOrSpace),
        ("a .", E::TrailingDotOrSpace),
        ("a..", E::TrailingDotOrSpace),
        ("file.txt.", E::TrailingDotOrSpace),
        ("file.txt ", E::TrailingDotOrSpace),
        ("file.txt...", E::TrailingDotOrSpace),
        ("dir./x", E::TrailingDotOrSpace),
        ("dir /x", E::TrailingDotOrSpace),
        ("x\\dir.\\y", E::TrailingDotOrSpace),
        (".regia /agent.toml", E::TrailingDotOrSpace),
        (".regia./agent.toml", E::TrailingDotOrSpace),
        // NTFS alternate data streams.
        ("file::$DATA", E::Prefix),
        ("file:s", E::Prefix),
        ("file:s:$DATA", E::Prefix),
        ("a\\file::$DATA", E::Prefix),
        ("dir::$INDEX_ALLOCATION\\x", E::Prefix),
        // Device names, in any case, with and without an extension, in any segment.
        ("CON", device("CON")),
        ("NUL", device("NUL")),
        ("COM1", device("COM1")),
        ("LPT1", device("LPT1")),
        ("PRN", device("PRN")),
        ("AUX", device("AUX")),
        ("con", device("con")),
        ("Nul", device("Nul")),
        ("com1", device("com1")),
        ("lpt1", device("lpt1")),
        ("CON.txt", device("CON.txt")),
        ("NUL.txt", device("NUL.txt")),
        ("COM1.txt", device("COM1.txt")),
        ("LPT1.txt", device("LPT1.txt")),
        ("NUL.tar.gz", device("NUL.tar.gz")),
        ("a\\NUL", device("NUL")),
        ("a/CON/b", device("CON")),
        ("CON ", device("CON ")),
        ("NUL.", device("NUL.")),
        ("COM1 .txt", device("COM1 .txt")),
        ("COM0", device("COM0")),
        ("LPT0", device("LPT0")),
        ("COM9", device("COM9")),
        ("COM\u{b9}", device("COM\u{b9}")),
        ("LPT\u{b3}", device("LPT\u{b3}")),
        ("com\u{b2}.txt", device("com\u{b2}.txt")),
        ("CONIN$", device("CONIN$")),
        ("conout$.log", device("conout$.log")),
        // Empty forms and NUL.
        ("", E::Empty),
        (".", E::Empty),
        (".\\", E::Empty),
        ("a\0b", E::Nul),
    ]
}

/// The accepted rows: the input and its clean `/`-separated form.
const ACCEPTED: [(&str, &str); 19] = [
    ("a\\b/c", "a/b/c"),
    ("a/b\\c.ron", "a/b/c.ron"),
    (".\\a", "a"),
    ("a\\.\\b", "a/b"),
    ("a//b", "a/b"),
    ("a b/c d.txt", "a b/c d.txt"),
    ("console.txt", "console.txt"),
    ("com10", "com10"),
    ("COM1x", "COM1x"),
    ("com", "com"),
    ("lpt", "lpt"),
    ("nullable/x", "nullable/x"),
    ("auxiliary.ron", "auxiliary.ron"),
    ("con_x", "con_x"),
    ("..a", "..a"),
    (".hidden", ".hidden"),
    (".regia/agent.toml", ".regia/agent.toml"),
    ("a.b.c", "a.b.c"),
    ("x/.y", "x/.y"),
];

#[test]
fn every_refused_row_is_refused_by_every_entry_point() {
    let root = temp_root("refused");
    let roots = [root.clone()];
    for (input, want) in refused() {
        assert_eq!(
            check_relative(input),
            Err(want.clone()),
            "check_relative {input:?}"
        );
        assert_eq!(
            resolve_read(&root, input),
            Err(want.clone()),
            "resolve_read {input:?}"
        );
        assert_eq!(
            resolve_write(&root, input),
            Err(want.clone()),
            "resolve_write {input:?}"
        );
        // On Linux only `/`-led inputs are absolute; those are held to the roots instead of to
        // `check_relative`, so they are refused for a different reason (see the next test).
        if !Path::new(input).is_absolute() {
            assert_eq!(
                resolve_in_roots(&roots, input),
                Err(want),
                "resolve_in_roots {input:?}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn every_accepted_row_keeps_its_segments_on_every_entry_point() {
    for (input, clean) in ACCEPTED {
        // Its own root per row: `a/b` is a file in one row and a folder in another.
        let root = temp_root("accepted");
        let roots = [root.clone()];
        let want: PathBuf = clean.split('/').collect();
        assert_eq!(
            check_relative(input).as_ref(),
            Ok(&want),
            "check_relative {input:?}"
        );
        assert_eq!(
            resolve_write(&root, input),
            Ok(root.join(&want)),
            "resolve_write {input:?}"
        );
        assert_eq!(
            resolve_in_roots(&roots, input),
            Ok(root.join(&want)),
            "resolve_in_roots {input:?}"
        );
        // Written through its clean path, the file reads back under the same spelling.
        let file = root.join(&want);
        std::fs::create_dir_all(file.parent().expect("under the root")).expect("dirs");
        std::fs::write(&file, b"x").expect("file");
        assert_eq!(
            resolve_read(&root, input),
            Ok(file.clone()),
            "resolve_read {input:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// An absolute path under an allowed root is allowed, but what follows the root is a project path
/// like any other: `C:\project\a.txt:stream` must not write a stream on a Windows host.
#[test]
fn what_follows_the_root_of_an_absolute_path_keeps_the_same_rules() {
    let root = temp_root("tail");
    let roots = [root.clone()];
    let under = |tail: &str| format!("{}/{tail}", root.display());
    for (tail, want) in [
        ("a.txt:stream", E::Prefix),
        ("file::$DATA", E::Prefix),
        ("sub/nul.txt", device("nul.txt")),
        ("COM\u{b9}", device("COM\u{b9}")),
        ("name.", E::TrailingDotOrSpace),
        ("dir /x", E::TrailingDotOrSpace),
    ] {
        assert_eq!(resolve_in_roots(&roots, &under(tail)), Err(want), "{tail:?}");
    }
    // The root's own spelling is not held to the rules, even when a wider root also holds the path.
    let base = temp_root("odd");
    let odd = base.join("nul.odd name.");
    std::fs::create_dir_all(&odd).expect("odd root");
    let odd_roots = [base.clone(), odd.clone()];
    assert!(resolve_in_roots(&odd_roots, &format!("{}/ok.txt", odd.display())).is_ok());
    assert!(resolve_in_roots(&odd_roots, &odd.display().to_string()).is_ok());
    assert!(resolve_in_roots(&roots, &under("ok/name.txt")).is_ok());
    for dir in [root, base] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// `\\server\share\x` asks Windows to connect to `server` and to offer the user's credentials the
/// moment anything stats it, so the root check has to come before the filesystem is touched.
#[test]
fn a_remote_form_is_recognised_without_touching_the_filesystem() {
    for remote in [
        "\\\\server\\share\\x",
        "\\\\server\\share",
        "//server/share/x",
        "\\\\?\\UNC\\server\\share\\x",
        "\\\\?\\C:\\x",
        "\\\\.\\pipe\\x",
        "\\/server/share",
        "/\\server\\share",
    ] {
        assert!(is_remote_form(remote), "{remote:?}");
    }
    for local in ["/tmp/x", "C:\\x", "C:/x", "a/b", "\\x", "", "/"] {
        assert!(!is_remote_form(local), "{local:?}");
    }
}

#[test]
fn a_remote_path_outside_the_roots_is_refused_as_remote() {
    let root = temp_root("remote");
    let roots = [root.clone()];
    assert_eq!(resolve_in_roots(&roots, "//server/share/x"), Err(E::Remote));
    assert_eq!(E::Remote.code(), "path_remote");
    // A project that really lives on a share keeps working: the path lies under an allowed root.
    // The spelling that is a remote form differs by host: `//tmp/x` on Unix, `\\?\C:\x` on Windows.
    let spelled = if cfg!(windows) {
        let canonical = root.canonicalize().expect("canonical root");
        format!("{}\\ok.txt", canonical.display())
    } else {
        format!("/{}/ok.txt", root.display())
    };
    assert!(is_remote_form(&spelled), "{spelled}");
    assert!(resolve_in_roots(&roots, &spelled).is_ok(), "{spelled}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A later root may come from a cloned project's `agent.toml`, so it cannot widen the rule to a whole
/// filesystem or make the check connect to a host it names.
#[test]
fn a_filesystem_root_or_a_remote_form_in_the_list_counts_for_nothing() {
    let root = temp_root("wide");
    let outside = temp_root("wide-outside");
    let target = outside.join("x.txt");
    let wide = [root.clone(), PathBuf::from("/")];
    assert_eq!(
        resolve_in_roots(&wide, &target.display().to_string()),
        Err(E::Escapes)
    );
    let remote = [root.clone(), PathBuf::from("//evil/share")];
    assert_eq!(resolve_in_roots(&remote, "//evil/share/x"), Err(E::Remote));
    // The project itself is still taken as given, and the others keep working.
    let both = [root.clone(), outside.clone()];
    assert_eq!(
        resolve_in_roots(&both, &target.display().to_string()),
        Ok(target.clone())
    );
    for dir in [root, outside] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(unix)]
#[test]
fn only_a_regular_file_under_its_limit_is_read() {
    use regia_core::project_path::read_regular_capped;
    let root = temp_root("capped");
    let file = root.join("f");
    std::fs::write(&file, b"12345").expect("file");
    assert_eq!(read_regular_capped(&file, 5).expect("at the limit"), b"12345");
    assert_eq!(
        read_regular_capped(&file, 4).map_err(|e| e.kind()),
        Err(std::io::ErrorKind::InvalidData)
    );
    // A link to a good file is still a link.
    let link = root.join("link");
    std::os::unix::fs::symlink(&file, &link).expect("link");
    assert_eq!(
        read_regular_capped(&link, 5).map_err(|e| e.kind()),
        Err(std::io::ErrorKind::InvalidData)
    );
    assert_eq!(
        read_regular_capped(&root, 5).map_err(|e| e.kind()),
        Err(std::io::ErrorKind::InvalidData)
    );
    // A FIFO with no writer: refused at once, the test would hang here otherwise.
    let fifo = root.join("fifo");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .is_ok_and(|s| s.success());
    if made {
        assert_eq!(
            read_regular_capped(&fifo, 5).map_err(|e| e.kind()),
            Err(std::io::ErrorKind::InvalidData)
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// What the rules assume about Windows, on Windows: these spellings reach another name, so a check on
/// the spelling would not be a check on the file. Each one is refused before any open.
#[cfg(windows)]
#[test]
fn windows_opens_these_names_as_the_rule_says() {
    let root = temp_root("real");
    std::fs::create_dir_all(root.join(".regia")).expect("dir");
    std::fs::write(root.join("a.txt"), b"plain").expect("file");
    std::fs::write(root.join(".regia").join("x"), b"inner").expect("file");
    for (spelled, want) in [
        ("a.txt.", "plain"),
        ("a.txt..", "plain"),
        ("a.txt ", "plain"),
        ("a.txt. ", "plain"),
        (".regia.\\x", "inner"),
    ] {
        let got = std::fs::read_to_string(root.join(spelled)).expect(spelled);
        assert_eq!(got, want, "{spelled:?}");
        assert_eq!(check_relative(spelled), Err(E::TrailingDotOrSpace), "{spelled:?}");
    }
    // An alternate stream: bytes in a file that its plain name does not show.
    std::fs::write(root.join("a.txt:s"), b"hidden").expect("stream");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).expect("plain"),
        "plain"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt:s")).expect("stream"),
        "hidden"
    );
    assert_eq!(check_relative("a.txt:s"), Err(E::Prefix));
    // A write to `NUL` succeeds and leaves no file.
    std::fs::write(root.join("NUL"), b"gone").expect("device");
    let mut listed = std::fs::read_dir(&root).expect("list").flatten();
    assert!(!listed.any(|e| e.file_name().eq_ignore_ascii_case("NUL")));
    assert_eq!(check_relative("NUL"), Err(device("NUL")));
    let _ = std::fs::remove_dir_all(&root);
}

/// The stated limit of the filesystem check: it holds when it runs. A process that can write inside
/// the project and swaps a checked folder for a link before the caller opens the path still
/// redirects that open. This pins the limit `docs/security/threat-model.md` states, so a change that
/// closes it (handle-relative opens) has to update the document as well.
#[cfg(unix)]
#[test]
fn a_folder_swapped_after_the_check_is_the_stated_limit() {
    let root = temp_root("race-root");
    let outside = temp_root("race-outside");
    std::fs::write(outside.join("x.txt"), b"outside").expect("outside");
    std::fs::create_dir_all(root.join("d")).expect("dir");
    std::fs::write(root.join("d/x.txt"), b"inside").expect("inside");
    let checked = resolve_read(&root, "d/x.txt").expect("inside the root");
    std::fs::remove_dir_all(root.join("d")).expect("remove");
    std::os::unix::fs::symlink(&outside, root.join("d")).expect("swap");
    assert_eq!(std::fs::read(&checked).expect("reads"), b"outside");
    // Any later check sees the link.
    assert_eq!(resolve_read(&root, "d/x.txt"), Err(E::Escapes));
    for dir in [root, outside] {
        let _ = std::fs::remove_dir_all(dir);
    }
}
