// SPDX-License-Identifier: MPL-2.0
//! The offline environment baker of the product viewer.
//!
//! ```text
//! regia_viewer_bake_env <input.hdr | --studio> <output.renv> [--face N] [--mips N] [--samples N]
//! regia_viewer_bake_env --dfg <output.bin>
//! ```

use std::process::ExitCode;

use regia_viewer::bake::{BakeOptions, StudioLights, bake, bake_dfg, parse_radiance_hdr};

fn run(args: &[String]) -> Result<String, String> {
    let usage = "usage: regia_viewer_bake_env <input.hdr | --studio> <output.renv> [--face N] [--mips N] \
                 [--samples N] | --dfg <output.bin>";
    let input = args.first().ok_or(usage)?;
    let output = args.get(1).ok_or(usage)?;
    if input == "--dfg" {
        let table = bake_dfg(regia_viewer::DFG_SIZE, 1024);
        write_atomic(output, &table)?;
        return Ok(format!("{output}: {} bytes", table.len()));
    }
    let mut options = BakeOptions::default();
    let mut rest = args[2..].iter();
    while let Some(flag) = rest.next() {
        let value: u32 = rest
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?
            .parse()
            .map_err(|_| format!("{flag} needs a number"))?;
        match flag.as_str() {
            "--face" => options.face_size = value,
            "--mips" => options.mip_count = value,
            "--samples" => options.samples = value,
            other => return Err(format!("unknown flag {other}\n{usage}")),
        }
    }
    let env = if input == "--studio" {
        bake(&StudioLights::default(), options)
    } else {
        let bytes = std::fs::read(input).map_err(|e| format!("{input}: {e}"))?;
        let image = parse_radiance_hdr(&bytes).map_err(|e| format!("{input}: {e}"))?;
        bake(&image, options)
    }
    .map_err(|e| e.to_string())?;
    let bytes = env.to_bytes();
    write_atomic(output, &bytes)?;
    Ok(format!(
        "{output}: {} bytes, {} per face, {} mips",
        bytes.len(),
        env.face_size(),
        env.mip_count()
    ))
}

fn write_atomic(path: &str, bytes: &[u8]) -> Result<(), String> {
    let tmp = format!("{path}.tmp{}", std::process::id());
    std::fs::write(&tmp, bytes).map_err(|e| format!("{tmp}: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{path}: {e}"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
