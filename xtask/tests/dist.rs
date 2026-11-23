// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask dist` end to end on Linux, without a cross toolchain: synthetic PE and ELF images
//! stand in for the built binaries (`--from`), and the bundle, manifest, archive and `--verify`
//! are checked (ADR-868).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use regia_test_utils::unique_temp_dir;
use serde_json::Value;
use xtask::dist::{self, Args, MANIFEST_FILE, Target};
use xtask::dist_archive;
use xtask::dist_binary;

/// A minimal PE32+ x64 console image whose import directory names `dlls`.
fn synthetic_pe(dlls: &[&str]) -> Vec<u8> {
    const SECTION_RVA: u32 = 0x1000;
    const SECTION_FILE: usize = 0x200;
    // Section contents: the descriptors (one per DLL plus the zero terminator), then the names.
    let mut section = vec![0u8; (dlls.len() + 1) * 20];
    for (i, dll) in dlls.iter().enumerate() {
        let name_rva = SECTION_RVA + section.len() as u32;
        section.extend_from_slice(dll.as_bytes());
        section.push(0);
        section[i * 20 + 12..i * 20 + 16].copy_from_slice(&name_rva.to_le_bytes());
    }
    let mut b = vec![0u8; SECTION_FILE];
    b[0..2].copy_from_slice(b"MZ");
    b[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    b[0x40..0x44].copy_from_slice(b"PE\0\0");
    let coff = 0x44;
    b[coff..coff + 2].copy_from_slice(&0x8664u16.to_le_bytes());
    b[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes()); // one section
    b[coff + 16..coff + 18].copy_from_slice(&240u16.to_le_bytes()); // PE32+ optional header, 16 dirs
    let opt = coff + 20;
    b[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes());
    b[opt + 68..opt + 70].copy_from_slice(&3u16.to_le_bytes()); // console
    b[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
    let import_dir = opt + 112 + 8; // data directory 1
    b[import_dir..import_dir + 4].copy_from_slice(&SECTION_RVA.to_le_bytes());
    b[import_dir + 4..import_dir + 8].copy_from_slice(&(((dlls.len() + 1) * 20) as u32).to_le_bytes());
    let table = opt + 240;
    b[table..table + 6].copy_from_slice(b".idata");
    b[table + 8..table + 12].copy_from_slice(&(section.len() as u32).to_le_bytes());
    b[table + 12..table + 16].copy_from_slice(&SECTION_RVA.to_le_bytes());
    b[table + 16..table + 20].copy_from_slice(&(section.len() as u32).to_le_bytes());
    b[table + 20..table + 24].copy_from_slice(&(SECTION_FILE as u32).to_le_bytes());
    b.extend_from_slice(&section);
    b
}

/// A minimal x86-64 ELF with `.dynstr`, `.dynamic` (`DT_NEEDED` for `needed`) and
/// `.gnu.version_r` (one `libc.so.6` entry requiring `glibc` versions such as `"GLIBC_2.17"`).
fn synthetic_elf(needed: &[&str], glibc: &[&str]) -> Vec<u8> {
    let mut dynstr = vec![0u8];
    let mut add = |s: &str| {
        let at = dynstr.len() as u32;
        dynstr.extend_from_slice(s.as_bytes());
        dynstr.push(0);
        at
    };
    let mut needed_at = Vec::new();
    for &name in needed {
        needed_at.push(add(name));
    }
    let libc_at = add("libc.so.6");
    let mut versions_at = Vec::new();
    for &version in glibc {
        versions_at.push(add(version));
    }

    let mut dynamic = Vec::new();
    for at in &needed_at {
        dynamic.extend_from_slice(&1u64.to_le_bytes()); // DT_NEEDED
        dynamic.extend_from_slice(&u64::from(*at).to_le_bytes());
    }
    dynamic.extend_from_slice(&[0u8; 16]); // DT_NULL

    let mut verneed = Vec::new();
    verneed.extend_from_slice(&1u16.to_le_bytes()); // vn_version
    verneed.extend_from_slice(&(glibc.len() as u16).to_le_bytes()); // vn_cnt
    verneed.extend_from_slice(&libc_at.to_le_bytes()); // vn_file
    verneed.extend_from_slice(&16u32.to_le_bytes()); // vn_aux
    verneed.extend_from_slice(&0u32.to_le_bytes()); // vn_next
    for (i, at) in versions_at.iter().enumerate() {
        verneed.extend_from_slice(&0u32.to_le_bytes()); // vna_hash (not checked)
        verneed.extend_from_slice(&0u16.to_le_bytes()); // vna_flags
        verneed.extend_from_slice(&(i as u16 + 2).to_le_bytes()); // vna_other
        verneed.extend_from_slice(&at.to_le_bytes()); // vna_name
        let next: u32 = if i + 1 < versions_at.len() { 16 } else { 0 };
        verneed.extend_from_slice(&next.to_le_bytes());
    }

    let dynstr_at = 64usize;
    let dynamic_at = (dynstr_at + dynstr.len()).next_multiple_of(8);
    let verneed_at = dynamic_at + dynamic.len();
    let shoff = (verneed_at + verneed.len()).next_multiple_of(8);
    let mut b = vec![0u8; shoff];
    b[0..4].copy_from_slice(b"\x7fELF");
    b[4] = 2; // 64-bit
    b[5] = 1; // little endian
    b[6] = 1; // version
    b[0x10..0x12].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    b[0x12..0x14].copy_from_slice(&62u16.to_le_bytes()); // x86-64
    b[0x14..0x18].copy_from_slice(&1u32.to_le_bytes());
    b[0x28..0x30].copy_from_slice(&(shoff as u64).to_le_bytes());
    b[0x34..0x36].copy_from_slice(&64u16.to_le_bytes());
    b[0x3A..0x3C].copy_from_slice(&64u16.to_le_bytes());
    b[0x3C..0x3E].copy_from_slice(&4u16.to_le_bytes());
    b[dynstr_at..dynstr_at + dynstr.len()].copy_from_slice(&dynstr);
    b[dynamic_at..dynamic_at + dynamic.len()].copy_from_slice(&dynamic);
    b[verneed_at..verneed_at + verneed.len()].copy_from_slice(&verneed);
    // Section headers: null, .dynstr (STRTAB), .dynamic (link 1), .gnu.version_r (link 1, info 1).
    let header = |kind: u32, offset: usize, size: usize, link: u32, info: u32| {
        let mut h = vec![0u8; 64];
        h[4..8].copy_from_slice(&kind.to_le_bytes());
        h[24..32].copy_from_slice(&(offset as u64).to_le_bytes());
        h[32..40].copy_from_slice(&(size as u64).to_le_bytes());
        h[40..44].copy_from_slice(&link.to_le_bytes());
        h[44..48].copy_from_slice(&info.to_le_bytes());
        h
    };
    b.extend(header(0, 0, 0, 0, 0));
    b.extend(header(3, dynstr_at, dynstr.len(), 0, 0));
    b.extend(header(6, dynamic_at, dynamic.len(), 1, 0));
    b.extend(header(0x6fff_fffe, verneed_at, verneed.len(), 1, 1));
    b
}

/// A fake repository root with the two license files, a `--from` directory holding `binary` as
/// `file_name`, and the output directory.
fn fixture(tag: &str, file_name: &str, binary: &[u8]) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let base = unique_temp_dir(tag);
    let root = base.join("repo");
    let from = base.join("built");
    let out = base.join("out");
    fs::create_dir_all(&root)?;
    fs::create_dir_all(&from)?;
    fs::write(root.join("LICENSE"), "Regia Engine license text\n")?;
    fs::write(root.join("THIRD_PARTY_NOTICES.md"), "# Third-party notices\n")?;
    fs::write(from.join(file_name), binary)?;
    Ok((root, from, out))
}

fn args(target: Target, from: &Path, out: &Path) -> Args {
    Args {
        target: Some(target),
        profile: "release".to_string(),
        from: Some(from.to_path_buf()),
        out: Some(out.to_path_buf()),
        ..Args::default()
    }
}

fn manifest_of(dir: &Path) -> Result<Value> {
    Ok(serde_json::from_str(&fs::read_to_string(
        dir.join(MANIFEST_FILE),
    )?)?)
}

#[test]
fn windows_bundle_lists_every_file_and_the_dll_imports() -> Result<()> {
    let pe = synthetic_pe(&[
        "KERNEL32.dll",
        "VCRUNTIME140.dll",
        "api-ms-win-crt-runtime-l1-1-0.dll",
    ]);
    let (root, from, out) = fixture("dist-win", "vfx_lab.exe", &pe)?;
    let plan = dist::plan(
        &args(Target::Windows, &from, &out),
        &root,
        &root.join("target"),
        "linux",
    )?;
    let staged = dist::stage(&plan, "abc1234", 1_792_611_413)?;

    let m = manifest_of(&staged.dir)?;
    assert_eq!(m, staged.manifest);
    assert_eq!(m["schema"], dist::SCHEMA);
    assert_eq!(m["kind"], "regia_dist");
    assert_eq!(m["target"], "windows");
    assert_eq!(m["git"], "abc1234");
    assert_eq!(m["build"]["tool"], "prebuilt");
    let paths: Vec<&str> = m["files"]
        .as_array()
        .map(|a| a.iter().filter_map(|f| f["path"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        paths,
        [
            "README.txt",
            "licenses/LICENSE",
            "licenses/THIRD_PARTY_NOTICES.md",
            "run-vfx_lab.cmd",
            "vfx_lab.exe"
        ],
        "sorted, the manifest itself not listed"
    );
    let exe = &m["files"][4];
    assert_eq!(exe["role"], "binary");
    assert_eq!(exe["bytes"], pe.len() as u64);
    assert_eq!(exe["blake3"], blake3::hash(&pe).to_hex().to_string());
    assert_eq!(m["licenses"][0], "licenses/LICENSE");

    let bin = &m["binaries"][0];
    assert_eq!(bin["subsystem"], "console");
    assert_eq!(bin["msvc_runtime"], "dynamic");
    let classes: Vec<(&str, &str)> = bin["imports"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|i| Some((i["dll"].as_str()?, i["class"].as_str()?)))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        classes,
        [
            ("KERNEL32.dll", "system"),
            ("VCRUNTIME140.dll", "msvc_runtime"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "system")
        ]
    );
    assert!(
        staged
            .warnings
            .iter()
            .any(|w| w.contains("Visual C++ Redistributable"))
    );
    assert_eq!(
        m["runtime_dirs"]["crash_dumps"]["path"],
        r"%LOCALAPPDATA%\RegiaEngine\<exe>\feedback"
    );

    // The README names the folders a tester sends back, with Windows separators.
    let readme = fs::read_to_string(staged.dir.join("README.txt"))?;
    assert!(readme.contains(r"%LOCALAPPDATA%\RegiaEngine\<exe>\feedback"));
    assert!(readme.contains(r"licenses\THIRD_PARTY_NOTICES.md"));
    assert!(readme.contains("REGIA_NATIVE_CRASH=1"));
    assert!(readme.contains("Visual C++ Redistributable"));

    // The zip holds the same files under one top folder, sorted, the manifest last, CRCs intact.
    let archive = staged
        .archive
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no archive"))?;
    assert!(archive.extension().is_some_and(|e| e == "zip"));
    let members = dist_archive::list_zip(&fs::read(&archive)?)?;
    let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
    let top = &plan.bundle;
    let expected: Vec<String> = [
        "README.txt",
        "licenses/LICENSE",
        "licenses/THIRD_PARTY_NOTICES.md",
        "run-vfx_lab.cmd",
        "vfx_lab.exe",
        MANIFEST_FILE,
    ]
    .iter()
    .map(|name| format!("{top}/{name}"))
    .collect();
    assert_eq!(names, expected);
    let exe_member = members
        .iter()
        .find(|m| m.name == format!("{top}/vfx_lab.exe"))
        .ok_or_else(|| anyhow::anyhow!("exe not in the zip"))?;
    assert_eq!(exe_member.data, pe);
    assert!(
        !out.join(format!("{top}.zip.tmp")).exists(),
        "no temp file left behind"
    );

    // Both the folder and the zip verify.
    assert_eq!(dist::verify(&staged.dir)?, Vec::<String>::new());
    assert_eq!(dist::verify(&archive)?, Vec::<String>::new());

    // The same inputs and mtime give the same archive bytes.
    let first = fs::read(&archive)?;
    dist::stage(&plan, "abc1234", 1_792_611_413)?;
    assert_eq!(fs::read(&archive)?, first, "deterministic zip");
    Ok(())
}

#[test]
fn verify_reports_changed_missing_and_unlisted_files() -> Result<()> {
    let pe = synthetic_pe(&["KERNEL32.dll"]);
    let (root, from, out) = fixture("dist-verify", "vfx_lab.exe", &pe)?;
    let plan = dist::plan(
        &args(Target::Windows, &from, &out),
        &root,
        &root.join("target"),
        "linux",
    )?;
    let staged = dist::stage(&plan, "abc1234", 0)?;
    assert_eq!(staged.manifest["binaries"][0]["msvc_runtime"], "static");
    assert!(staged.warnings.is_empty());

    fs::write(staged.dir.join("licenses/LICENSE"), "edited\n")?;
    fs::write(staged.dir.join("extra.dll"), "x")?;
    fs::remove_file(staged.dir.join("README.txt"))?;
    let problems = dist::verify(&staged.dir)?;
    assert_eq!(problems.len(), 3, "{problems:#?}");
    assert!(problems.iter().any(|p| p.starts_with("licenses/LICENSE:")));
    assert!(
        problems
            .iter()
            .any(|p| p.contains("README.txt: listed in the manifest but missing"))
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("extra.dll: in the bundle but not in the manifest"))
    );

    // Restaging over the edited folder works: it has a manifest, so it is ours to replace.
    let restaged = dist::stage(&plan, "abc1234", 0)?;
    assert_eq!(dist::verify(&restaged.dir)?, Vec::<String>::new());
    Ok(())
}

#[test]
fn an_unknown_dll_fails_the_windows_bundle() -> Result<()> {
    let pe = synthetic_pe(&["KERNEL32.dll", "mystery_runtime.dll"]);
    let (root, from, out) = fixture("dist-unknown-dll", "vfx_lab.exe", &pe)?;
    let plan = dist::plan(
        &args(Target::Windows, &from, &out),
        &root,
        &root.join("target"),
        "linux",
    )?;
    let err = dist::stage(&plan, "abc1234", 0).err().map(|e| format!("{e:#}"));
    let err = err.unwrap_or_default();
    assert!(err.contains("mystery_runtime.dll"), "{err}");
    assert!(
        !out.join(&plan.bundle).join(MANIFEST_FILE).exists(),
        "no manifest on failure"
    );
    // A failed run can be redone: the half-staged folder carries the staging marker.
    let fixed = synthetic_pe(&["KERNEL32.dll"]);
    fs::write(from.join("vfx_lab.exe"), fixed)?;
    let staged = dist::stage(&plan, "abc1234", 0)?;
    assert_eq!(dist::verify(&staged.dir)?, Vec::<String>::new());
    Ok(())
}

#[test]
fn a_foreign_folder_in_the_bundle_path_is_not_deleted() -> Result<()> {
    let pe = synthetic_pe(&["KERNEL32.dll"]);
    let (root, from, out) = fixture("dist-foreign", "vfx_lab.exe", &pe)?;
    let plan = dist::plan(
        &args(Target::Windows, &from, &out),
        &root,
        &root.join("target"),
        "linux",
    )?;
    let foreign = out.join(&plan.bundle);
    fs::create_dir_all(&foreign)?;
    fs::write(foreign.join("notes.txt"), "mine")?;
    assert!(dist::stage(&plan, "abc1234", 0).is_err());
    assert_eq!(fs::read_to_string(foreign.join("notes.txt"))?, "mine");
    Ok(())
}

#[test]
fn deck_bundle_checks_glibc_and_keeps_the_executable_bit() -> Result<()> {
    let elf = synthetic_elf(
        &["libc.so.6", "libm.so.6"],
        &["GLIBC_2.2.5", "GLIBC_2.17", "GLIBC_2.31"],
    );
    let report = dist_binary::read_elf(&elf)?;
    assert_eq!(report.needed, ["libc.so.6", "libm.so.6"]);
    assert_eq!(report.max_glibc(), Some([2u32, 31].as_slice()));

    let (root, from, out) = fixture("dist-deck", "vfx_lab", &elf)?;
    let plan = dist::plan(
        &args(Target::Deck, &from, &out),
        &root,
        &root.join("target"),
        "linux",
    )?;
    let staged = dist::stage(&plan, "abc1234", 1_792_611_413)?;
    let m = &staged.manifest;
    assert_eq!(m["glibc_floor"], "2.31");
    assert_eq!(m["binaries"][0]["glibc_max"], "2.31");
    assert_eq!(m["binaries"][0]["outside_runtime"], serde_json::json!([]));
    assert_eq!(
        m["runtime_dirs"]["pipeline_cache"]["path"],
        "~/.cache/RegiaEngine/<exe>/pipelines"
    );
    let readme = fs::read_to_string(staged.dir.join("README.txt"))?;
    assert!(readme.contains("~/.local/share/RegiaEngine/<exe>/feedback"));
    assert!(readme.contains("./vfx_lab --deck-check"));

    let archive = staged
        .archive
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no archive"))?;
    assert!(archive.extension().is_some_and(|e| e == "tar"));
    let members = dist_archive::list_tar(&fs::read(&archive)?)?;
    let binary = members
        .iter()
        .find(|m| m.name == format!("{}/vfx_lab", plan.bundle))
        .ok_or_else(|| anyhow::anyhow!("binary not in the tar"))?;
    assert_eq!(binary.mode, Some(0o755));
    assert_eq!(binary.data, elf);
    let license = members
        .iter()
        .find(|m| m.name.ends_with("licenses/LICENSE"))
        .ok_or_else(|| anyhow::anyhow!("license not in the tar"))?;
    assert_eq!(license.mode, Some(0o644));
    assert_eq!(dist::verify(&archive)?, Vec::<String>::new());
    assert_eq!(dist::verify(&staged.dir)?, Vec::<String>::new());

    // GNU tar or bsdtar, when the machine has one, lists the same members.
    if let Ok(output) = std::process::Command::new("tar")
        .arg("-tf")
        .arg(&archive)
        .output()
        && output.status.success()
    {
        let listed = String::from_utf8_lossy(&output.stdout);
        let ours: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        let theirs: Vec<&str> = listed.lines().collect();
        assert_eq!(theirs, ours);
    }

    // A binary that lost its executable bit fails the folder check.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(staged.dir.join("vfx_lab"), fs::Permissions::from_mode(0o644))?;
        let problems = dist::verify(&staged.dir)?;
        assert!(
            problems.iter().any(|p| p.contains("not executable")),
            "{problems:#?}"
        );
    }
    Ok(())
}

#[test]
fn deck_bundle_refuses_a_newer_glibc() -> Result<()> {
    let elf = synthetic_elf(&["libc.so.6", "libssl.so.3"], &["GLIBC_2.17", "GLIBC_2.34"]);
    let (root, from, out) = fixture("dist-deck-glibc", "vfx_lab", &elf)?;
    let plan = dist::plan(
        &args(Target::Deck, &from, &out),
        &root,
        &root.join("target"),
        "linux",
    )?;
    let err = dist::stage(&plan, "abc1234", 0).err().map(|e| format!("{e:#}"));
    let err = err.unwrap_or_default();
    assert!(err.contains("needs glibc 2.34"), "{err}");
    assert!(err.contains("x86_64-unknown-linux-gnu.2.31"), "{err}");
    // A library outside the runtime is a note, not a failure.
    let check = dist::check_binary(
        Target::Deck,
        "vfx_lab",
        &synthetic_elf(&["libc.so.6", "libssl.so.3"], &["GLIBC_2.17"]),
        &[],
    )?;
    assert!(check.problems.is_empty());
    assert!(check.warnings.iter().any(|w| w.contains("libssl.so.3")));
    Ok(())
}

#[test]
fn a_windows_exe_in_a_deck_bundle_is_refused() -> Result<()> {
    let pe = synthetic_pe(&["KERNEL32.dll"]);
    assert!(dist::check_binary(Target::Deck, "vfx_lab", &pe, &[]).is_err());
    let elf = synthetic_elf(&["libc.so.6"], &["GLIBC_2.17"]);
    assert!(dist::check_binary(Target::Windows, "vfx_lab.exe", &elf, &[]).is_err());
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn this_test_binary_reads_as_a_linux_elf() -> Result<()> {
    // A real linker's output, not a synthetic image: the reader finds libc and its versions.
    let bytes = fs::read(std::env::current_exe()?)?;
    let report = dist_binary::read_elf(&bytes)?;
    assert!(
        report.needed.iter().any(|n| n == "libc.so.6"),
        "{:?}",
        report.needed
    );
    assert!(report.max_glibc().is_some_and(|v| v >= [2u32, 2, 5].as_slice()));
    Ok(())
}

#[test]
fn shipped_pipeline_caches_are_validated_and_renamed() -> Result<()> {
    use regia_render::pipeline_cache::{PipelineCacheKey, WGPU_VERSION, encode, engine_shader_hash};
    let pe = synthetic_pe(&["KERNEL32.dll"]);
    let (root, from, out) = fixture("dist-fpc", "vfx_lab.exe", &pe)?;
    let caches = from.join("caches");
    fs::create_dir_all(&caches)?;
    let key = PipelineCacheKey {
        backend: "Vulkan".into(),
        vendor: 0x10de,
        device: 0x2c02,
        driver: "NVIDIA".into(),
        driver_info: "590.10".into(),
        wgpu_version: WGPU_VERSION.into(),
        shader_hash: engine_shader_hash(),
    };
    fs::write(caches.join("small.fpc"), encode(&key, &[1u8; 100]))?;
    fs::write(caches.join("large.fpc"), encode(&key, &[2u8; 300]))?;
    fs::write(caches.join("notes.txt"), "ignored")?;
    let mut a = args(Target::Windows, &from, &out);
    a.pipeline_caches = Some(caches.clone());
    let plan = dist::plan(&a, &root, &root.join("target"), "linux")?;
    let shipped: Vec<&dist::PlanFile> = plan
        .files
        .iter()
        .filter(|f| f.role == dist::Role::PipelineCache)
        .collect();
    assert_eq!(shipped.len(), 1, "one key, the larger file wins");
    assert_eq!(
        shipped[0].dest,
        format!("content/pipelines/{}", key.shipped_file_name())
    );
    assert!(
        shipped[0]
            .source
            .as_ref()
            .is_some_and(|p| p.ends_with("large.fpc"))
    );
    let staged = dist::stage(&plan, "abc1234", 0)?;
    assert!(
        staged
            .dir
            .join("content/pipelines")
            .join(key.shipped_file_name())
            .is_file()
    );
    assert_eq!(dist::verify(&staged.dir)?, Vec::<String>::new());

    // A torn cache is an error, never shipped.
    let mut torn = encode(&key, &[3u8; 50]);
    let last = torn.len() - 1;
    torn[last] ^= 0xff;
    fs::write(caches.join("torn.fpc"), torn)?;
    assert!(dist::plan(&a, &root, &root.join("target"), "linux").is_err());
    Ok(())
}

/// The paths of a manifest's `files[]`.
fn file_paths(m: &Value) -> Vec<String> {
    m["files"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| f["path"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_windows_dry_run_manifest_matches_the_staged_layout() -> Result<()> {
    // The dry run on Linux: no exe built yet, the license files are there.
    let pe = synthetic_pe(&["KERNEL32.dll"]);
    let (root, from, out) = fixture("dist-preview", "vfx_lab.exe", &pe)?;
    let empty = from.with_file_name("not-built");
    fs::create_dir_all(&empty)?;
    let plan = dist::plan(
        &args(Target::Windows, &empty, &out),
        &root,
        &root.join("target"),
        "linux",
    )?;
    let preview = dist::preview_manifest(&plan, "abc1234")?;
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["schema"], dist::SCHEMA);
    assert_eq!(preview["kind"], "regia_dist");
    assert_eq!(preview["target"], "windows");
    assert_eq!(preview["triple"], "x86_64-pc-windows-msvc");
    assert_eq!(
        preview["archive"],
        format!("regia-windows-{}.zip", dist::engine_version())
    );
    assert_eq!(preview["missing"], serde_json::json!(["vfx_lab.exe"]));
    // The installer-less folder: the exe at the top, the notices under licenses/.
    assert_eq!(
        file_paths(&preview),
        [
            "README.txt",
            "licenses/LICENSE",
            "licenses/THIRD_PARTY_NOTICES.md",
            "run-vfx_lab.cmd",
            "vfx_lab.exe"
        ]
    );
    let exe = &preview["files"][4];
    assert_eq!(exe["bytes"], Value::Null);
    assert_eq!(exe["blake3"], Value::Null);
    assert_eq!(exe["mode"], "755");
    let notices = &preview["files"][2];
    assert_eq!(
        notices["blake3"],
        blake3::hash(b"# Third-party notices\n").to_hex().to_string()
    );
    assert_eq!(preview["licenses"].as_array().map(Vec::len), Some(2));
    // Planning and previewing wrote nothing.
    assert!(!out.exists());

    // The real bundle of the same plan (with the exe present) lists the same files, the same
    // license hashes and the same top-level keys apart from the preview's own two.
    let plan = dist::plan(
        &args(Target::Windows, &from, &out),
        &root,
        &root.join("target"),
        "linux",
    )?;
    let staged = dist::stage(&plan, "abc1234", 1_792_611_413)?;
    assert_eq!(file_paths(&staged.manifest), file_paths(&preview));
    assert_eq!(staged.manifest["files"][1], preview["files"][1]);
    assert_eq!(staged.manifest["files"][2], preview["files"][2]);
    let keys = |m: &Value| -> Vec<String> {
        m.as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    };
    let mut preview_keys = keys(&preview);
    preview_keys.retain(|k| !matches!(k.as_str(), "dry_run" | "missing" | "archive"));
    assert_eq!(preview_keys, keys(&staged.manifest));

    // A preview dropped into a bundle folder is refused by --verify.
    fs::write(
        staged.dir.join(MANIFEST_FILE),
        serde_json::to_string_pretty(&preview)?,
    )?;
    let problems = dist::verify(&staged.dir)?;
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("--dry-run preview"), "{problems:?}");
    Ok(())
}

#[test]
fn manifest_out_needs_dry_run() {
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        dist: Args,
    }
    assert!(Cli::try_parse_from(["dist", "--target", "windows", "--manifest-out", "m.json"]).is_err());
    let cli = Cli::try_parse_from([
        "dist",
        "--target",
        "windows",
        "--dry-run",
        "--manifest-out",
        "m.json",
    ]);
    assert!(cli.is_ok_and(|c| c.dist.manifest_out == Some(PathBuf::from("m.json"))));
}

#[test]
fn dry_run_plans_both_targets_on_linux() -> Result<()> {
    let root = unique_temp_dir("dist-dry");
    for target in [Target::Windows, Target::Deck] {
        let a = Args {
            target: Some(target),
            profile: "release".to_string(),
            ..Args::default()
        };
        let plan = dist::plan(&a, &root, &root.join("target"), "linux")?;
        let json = dist::plan_json(&plan);
        let command = json["build"]["command"].as_str().unwrap_or_default().to_string();
        match target {
            Target::Windows => assert!(command.starts_with("cargo xwin build"), "{command}"),
            Target::Deck => assert!(command.contains("x86_64-unknown-linux-gnu.2.31"), "{command}"),
        }
        assert!(json["runtime_dirs"]["crash_dumps"]["path"].is_string());
        // Planning wrote nothing.
        assert!(!root.join("target").exists());
    }
    Ok(())
}

/// A fake PE the DLL set and the DXC pair are checked as (machine x64, no imports).
fn fake_dll() -> Vec<u8> {
    synthetic_pe(&[])
}

/// A directory holding `names` as fake x64 DLLs, plus a text file the DLL scan must ignore.
fn dll_dir(tag: &str, names: &[&str]) -> Result<PathBuf> {
    let dir = unique_temp_dir(tag);
    fs::create_dir_all(&dir)?;
    for name in names {
        fs::write(dir.join(name), fake_dll())?;
    }
    fs::write(dir.join("notes.txt"), "not a dll")?;
    Ok(dir)
}

#[test]
fn the_full_windows_layout_with_dlls_dxc_and_a_launcher() -> Result<()> {
    let pe = synthetic_pe(&["KERNEL32.dll", "steam_api64.dll"]);
    let (root, from, out) = fixture("dist-layout", "vfx_lab.exe", &pe)?;
    fs::write(from.join("vfx_lab.pdb"), b"fake pdb")?;
    let dlls = dll_dir("dist-layout-dlls", &["steam_api64.dll", "Extra.DLL"])?;
    let dxc = dll_dir("dist-layout-dxc", &["dxcompiler.dll", "dxil.dll"])?;
    let mut a = args(Target::Windows, &from, &out);
    a.dlls = Some(dlls);
    a.dxc = Some(dxc);

    // The dry run lists the whole layout on Linux and writes nothing.
    let plan = dist::plan(&a, &root, &root.join("target"), "linux")?;
    let preview = dist::preview_manifest(&plan, "abc1234")?;
    assert_eq!(
        file_paths(&preview),
        [
            "Extra.DLL",
            "README.txt",
            "dxcompiler.dll",
            "dxil.dll",
            "licenses/LICENSE",
            "licenses/THIRD_PARTY_NOTICES.md",
            "run-vfx_lab.cmd",
            "steam_api64.dll",
            "vfx_lab.exe"
        ]
    );
    assert_eq!(preview["dx12"]["compiler"], "bundled_dxc");
    assert!(!out.exists());

    let staged = dist::stage(&plan, "abc1234", 1_792_611_413)?;
    assert_eq!(file_paths(&staged.manifest), file_paths(&preview));
    let roles: Vec<(String, String)> = staged.manifest["files"]
        .as_array()
        .map(|f| {
            f.iter()
                .filter_map(|e| Some((e["path"].as_str()?.to_string(), e["role"].as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let role = |name: &str| roles.iter().find(|(p, _)| p == name).map(|(_, r)| r.as_str());
    assert_eq!(role("dxil.dll"), Some("dxc"));
    assert_eq!(role("dxcompiler.dll"), Some("dxc"));
    assert_eq!(role("steam_api64.dll"), Some("dll"));
    assert_eq!(role("Extra.DLL"), Some("dll"));
    assert_eq!(role("run-vfx_lab.cmd"), Some("launcher"));
    // A DLL the exe imports and the bundle ships is classed bundled, not an error.
    let imports = &staged.manifest["binaries"][0]["imports"];
    assert_eq!(imports[1]["dll"], "steam_api64.dll");
    assert_eq!(imports[1]["class"], "bundled");
    // The preview hashes the launcher exactly as stage writes it.
    let launcher = fs::read(staged.dir.join("run-vfx_lab.cmd"))?;
    assert_eq!(launcher, dist::launcher_script("vfx_lab").into_bytes());
    let entry = |m: &Value| {
        m["files"]
            .as_array()
            .and_then(|a| a.iter().find(|f| f["path"] == "run-vfx_lab.cmd").cloned())
    };
    assert_eq!(entry(&preview), entry(&staged.manifest));
    // The symbols stay out of the bundle.
    assert_eq!(staged.manifest["symbols"][0]["path"], "symbols/vfx_lab.pdb");
    assert!(!staged.dir.join("vfx_lab.pdb").exists());
    let readme = fs::read_to_string(staged.dir.join("README.txt"))?;
    assert!(readme.contains("dxcompiler.dll and dxil.dll are next to the exe"));
    assert!(readme.contains("run-vfx_lab.cmd"));
    assert_eq!(dist::verify(&staged.dir)?, Vec::<String>::new());
    let archive = staged.archive.ok_or_else(|| anyhow::anyhow!("no archive"))?;
    assert_eq!(dist::verify(&archive)?, Vec::<String>::new());
    Ok(())
}

#[test]
fn the_launcher_script_has_crlf_ends_and_passes_arguments() {
    let text = dist::launcher_script("vfx_lab");
    assert!(text.starts_with("@echo off\r\n"));
    assert!(text.ends_with("\r\n"));
    assert!(
        !text.replace("\r\n", "").contains('\n'),
        "every line ends in CRLF"
    );
    assert!(text.contains("cd /d \"%~dp0\""));
    assert!(text.contains("\"%~dp0vfx_lab.exe\" %*"));
    assert!(text.contains("REGIA_NATIVE_CRASH=1"));
    assert!(text.contains("exit /b %CODE%"));
    assert_eq!(dist::launcher_name("vfx_lab"), "run-vfx_lab.cmd");
}

#[test]
fn a_bundle_without_dxc_says_which_compiler_it_relies_on() -> Result<()> {
    let (root, from, out) = fixture("dist-nodxc", "vfx_lab.exe", &synthetic_pe(&["KERNEL32.dll"]))?;
    let mut a = args(Target::Windows, &from, &out);
    a.no_launcher = true;
    let plan = dist::plan(&a, &root, &root.join("target"), "linux")?;
    let staged = dist::stage(&plan, "abc1234", 1_792_611_413)?;
    assert_eq!(staged.manifest["dx12"]["compiler"], "engine");
    assert!(!file_paths(&staged.manifest).iter().any(|p| p.ends_with(".cmd")));
    let readme = fs::read_to_string(staged.dir.join("README.txt"))?;
    assert!(readme.contains("No dxcompiler.dll or dxil.dll is bundled"));
    Ok(())
}

#[test]
fn a_dxc_directory_needs_both_files_and_x64_images() -> Result<()> {
    let (root, from, out) = fixture("dist-dxc-bad", "vfx_lab.exe", &synthetic_pe(&["KERNEL32.dll"]))?;
    // dxil.dll missing.
    let mut a = args(Target::Windows, &from, &out);
    a.dxc = Some(dll_dir("dist-dxc-half", &["dxcompiler.dll"])?);
    let err = dist::plan(&a, &root, &root.join("target"), "linux")
        .err()
        .map(|e| e.to_string());
    assert!(err.is_some_and(|e| e.contains("dxil.dll is missing")));
    // A DLL that is not a PE image fails the bundle.
    let bad = unique_temp_dir("dist-dxc-notpe");
    fs::create_dir_all(&bad)?;
    fs::write(bad.join("dxcompiler.dll"), b"MZ")?;
    fs::write(bad.join("dxil.dll"), fake_dll())?;
    let mut a = args(Target::Windows, &from, &out);
    a.dxc = Some(bad);
    let plan = dist::plan(&a, &root, &root.join("target"), "linux")?;
    let err = dist::stage(&plan, "abc1234", 0).err().map(|e| format!("{e:#}"));
    assert!(err.is_some_and(|e| e.contains("dxcompiler.dll: not a PE image")));
    Ok(())
}

#[test]
fn dll_flags_are_windows_only_and_dll_names_cannot_collide() -> Result<()> {
    let root = unique_temp_dir("dist-flags");
    let mut a = Args {
        target: Some(Target::Deck),
        profile: "release".to_string(),
        dxc: Some(root.clone()),
        ..Args::default()
    };
    assert!(dist::plan(&a, &root, &root.join("target"), "linux").is_err());
    a.dxc = None;
    a.no_launcher = true;
    assert!(dist::plan(&a, &root, &root.join("target"), "linux").is_err());

    // The same name in --dxc and --dlls is refused.
    let both = dll_dir("dist-flags-dll", &["dxil.dll", "dxcompiler.dll"])?;
    let w = Args {
        target: Some(Target::Windows),
        profile: "release".to_string(),
        dxc: Some(both.clone()),
        dlls: Some(both),
        ..Args::default()
    };
    assert!(dist::plan(&w, &root, &root.join("target"), "linux").is_err());
    // The Deck bundle has no launcher.
    let d = Args {
        target: Some(Target::Deck),
        profile: "release".to_string(),
        ..Args::default()
    };
    let plan = dist::plan(&d, &root, &root.join("target"), "linux")?;
    assert!(plan.files.iter().all(|f| f.role != dist::Role::Launcher));
    Ok(())
}
