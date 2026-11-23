// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/stack_dump.rs,
// commit d6ed37677, regia-absorb-1), ADR-8012; adapted under ADR-8156.
//! Adapted ELF symbol-table lookup from DO27 stack_dump.rs:198-287,
//! d6ed37677 (regia-absorb-1), owner-authorized P-08 copy-and-adapt, class P3.
//! Adaptations: bounded checked indexes, PT_LOAD address translation, no subprocesses.

use super::{SampleOutcome, ThreadSample};
use std::collections::BTreeMap;
use std::os::unix::fs::FileExt;

struct Module {
    start: usize,
    end: usize,
    base: usize,
    path: String,
}

fn modules() -> Vec<Module> {
    let text = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let mut out = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(range), Some(_perms), Some(offset), Some(_dev), Some(_inode)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        let path = fields.collect::<Vec<_>>().join(" ");
        if !path.starts_with('/') {
            continue;
        }
        let Some((a, b)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end), Ok(offset)) = (
            usize::from_str_radix(a, 16),
            usize::from_str_radix(b, 16),
            usize::from_str_radix(offset, 16),
        ) else {
            continue;
        };
        let Some(base) = start.checked_sub(offset) else {
            continue;
        };
        out.push(Module {
            start,
            end,
            base,
            path,
        });
    }
    out
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<usize> {
    Some(u16::from_le_bytes(bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?) as usize)
}
fn u32_at(bytes: &[u8], offset: usize) -> Option<usize> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?) as usize)
}
fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
}

struct Symbols {
    loads: Vec<ExecutableLoad>,
    functions: Vec<(usize, usize, String)>,
}

struct ExecutableLoad {
    file_offset: usize,
    file_size: usize,
    virtual_address: usize,
}

impl ExecutableLoad {
    fn address(&self, file_offset: usize) -> Option<usize> {
        let delta = file_offset.checked_sub(self.file_offset)?;
        if delta >= self.file_size {
            return None;
        }
        self.virtual_address.checked_add(delta)
    }
}

fn elf_functions(path: &str, max_bytes: usize, max_name_bytes: usize) -> Option<Symbols> {
    let file = std::fs::File::open(path).ok()?;
    let file_size = file.metadata().ok()?.len();
    let read = |offset: u64, len: usize| -> Option<Vec<u8>> {
        if len > max_bytes || offset.checked_add(len as u64)? > file_size {
            return None;
        }
        let mut buffer = Vec::new();
        buffer.try_reserve_exact(len).ok()?;
        buffer.resize(len, 0);
        file.read_exact_at(&mut buffer, offset).ok()?;
        Some(buffer)
    };
    let head = read(0, 64)?;
    if &head[..4] != b"\x7fELF" || head[4] != 2 || head[5] != 1 {
        return None;
    }
    if ![2, 3].contains(&u16_at(&head, 16)?) {
        return None;
    }
    let program_size = u16_at(&head, 54)?;
    if program_size < 56 {
        return None;
    }
    let programs = read(u64_at(&head, 32)?, program_size.checked_mul(u16_at(&head, 56)?)?)?;
    let mut loads = Vec::new();
    for program in programs.chunks(program_size) {
        // Only executable PT_LOAD bytes can contain a sampled instruction.
        if u32_at(program, 0)? != 1 || u32_at(program, 4)? & 1 == 0 {
            continue;
        }
        let offset = u64_at(program, 8)?;
        let size = u64_at(program, 32)?;
        if size > u64_at(program, 40)? || offset.checked_add(size)? > file_size {
            return None;
        }
        loads.push(ExecutableLoad {
            file_offset: usize::try_from(offset).ok()?,
            file_size: usize::try_from(size).ok()?,
            virtual_address: usize::try_from(u64_at(program, 16)?).ok()?,
        });
    }
    let (offset, entry_size, count) = (u64_at(&head, 40)?, u16_at(&head, 58)?, u16_at(&head, 60)?);
    if entry_size < 64 || count == 0 {
        return None;
    }
    let table = read(offset, entry_size.checked_mul(count)?)?;
    let section = |index: usize| -> Option<(usize, u64, usize, usize, usize)> {
        if index >= count {
            return None;
        }
        let start = index.checked_mul(entry_size)?;
        let section = table.get(start..start.checked_add(64)?)?;
        Some((
            u32_at(section, 4)?,
            u64_at(section, 24)?,
            usize::try_from(u64_at(section, 32)?).ok()?,
            u32_at(section, 40)?,
            usize::try_from(u64_at(section, 56)?).ok()?,
        ))
    };
    let symbols = (0..count)
        .filter_map(section)
        .find(|s| s.0 == 2)
        .or_else(|| (0..count).filter_map(section).find(|s| s.0 == 11))?;
    if symbols.4 != 24 || !symbols.2.is_multiple_of(24) {
        return None;
    }
    let names = section(symbols.3)?;
    if names.0 != 3 {
        return None;
    }
    let (bytes, strings) = (read(symbols.1, symbols.2)?, read(names.1, names.2)?);
    let mut functions = Vec::new();
    let mut name_budget = max_bytes;
    for sym in bytes.as_chunks::<24>().0 {
        let (name, info, value, size) = (
            u32_at(sym, 0)?,
            sym[4],
            usize::try_from(u64_at(sym, 8)?).ok()?,
            usize::try_from(u64_at(sym, 16)?).ok()?,
        );
        if info & 0xf != 2 || value == 0 || name >= strings.len() || u16_at(sym, 6)? == 0 {
            continue;
        }
        if name_budget == 0 || max_name_bytes == 0 {
            break;
        }
        let length = (strings.len() - name).min(max_name_bytes).min(name_budget);
        let tail = &strings[name..name + length];
        let Some(end) = tail.iter().position(|&byte| byte == 0) else {
            name_budget -= length;
            continue;
        };
        name_budget -= end + 1;
        functions.push((value, size, String::from_utf8_lossy(&tail[..end]).into_owned()));
    }
    functions.sort_unstable_by_key(|function| function.0);
    Some(Symbols { loads, functions })
}

fn function_at(functions: &[(usize, usize, String)], offset: usize) -> Option<(&str, usize)> {
    let index = functions
        .partition_point(|function| function.0 <= offset)
        .checked_sub(1)?;
    let (start, size, name) = &functions[index];
    let delta = offset.checked_sub(*start)?;
    // A zero-sized symbol has no known extent; label only its exact address.
    (delta < *size || (*size == 0 && delta == 0)).then_some((name.as_str(), delta))
}

pub(super) fn symbolize(threads: &mut [ThreadSample], max_bytes: usize, max_name_bytes: usize) {
    use std::fmt::Write;
    let maps = modules();
    let mut addresses = BTreeMap::new();
    for thread in threads.iter() {
        if let SampleOutcome::Captured(frames) = &thread.outcome {
            for &ip in frames {
                addresses.entry(ip).or_insert_with(String::new);
            }
        }
    }
    // One module's symbol tables at a time, not an entire process worth of tables.
    let mut paths: Vec<_> = maps.iter().map(|module| module.path.as_str()).collect();
    paths.sort_unstable();
    paths.dedup();
    for path in paths {
        let relevant: Vec<_> = addresses
            .keys()
            .copied()
            .filter_map(|ip| {
                maps.iter()
                    .find(|module| {
                        module.path == path
                            && ip.saturating_sub(1) >= module.start
                            && ip.saturating_sub(1) < module.end
                    })
                    .map(|module| (ip, ip.saturating_sub(1).saturating_sub(module.base)))
            })
            .collect();
        if relevant.is_empty() {
            continue;
        }
        let symbols = elf_functions(path, max_bytes, max_name_bytes);
        for (ip, relative) in relevant {
            // /proc/maps supplies a file offset, not an ELF virtual address.
            // PT_LOAD p_vaddr and p_offset can differ even in a PIE (e.g. lld).
            let address = symbols
                .as_ref()
                .and_then(|s| s.loads.iter().find_map(|load| load.address(relative)));
            let mut label = if let Some(address) = address {
                format!("{path}+{address:#x}")
            } else {
                format!("{path} file+{relative:#x} (ip {ip:#x})")
            };
            if let Some((name, delta)) = symbols
                .as_ref()
                .and_then(|s| address.and_then(|address| function_at(&s.functions, address)))
            {
                let _ = write!(label, " {name}+{delta:#x}");
            }
            addresses.insert(ip, label);
        }
    }
    for thread in threads {
        if let SampleOutcome::Captured(frames) = &thread.outcome {
            for (index, ip) in frames.iter().enumerate() {
                let label = addresses
                    .get(ip)
                    .filter(|label| !label.is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("{ip:#x}"));
                let _ = writeln!(thread.stack, "  #{index:<2} {label}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn elf_load_virtual_addresses_are_not_assumed_to_equal_file_offsets() {
        // Linker layout from the acceptance failure: executable bytes start one
        // page later in virtual-address space than their offset in the file.
        for virtual_address in [0xa473d0, 0x40a473d0] {
            let load = ExecutableLoad {
                file_offset: 0xa463d0,
                file_size: 0xcbff0,
                virtual_address,
            };
            assert_eq!(load.address(0xa487ff), Some(virtual_address + 0x242f));
            assert_eq!(load.address(0xa463cf), None);
            assert_eq!(load.address(0xb123c0), None);
        }
        let overflowing = ExecutableLoad {
            file_offset: 0,
            file_size: 2,
            virtual_address: usize::MAX,
        };
        assert_eq!(overflowing.address(1), None);
    }

    #[test]
    fn malformed_elf_link_and_table_bounds_are_rejected() -> io::Result<()> {
        let dir = regia_test_utils::TempDir::new("stack-elf");
        let path = dir.join("bad.elf");
        let mut bytes = vec![0; 192];
        bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
        bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
        bytes[40..48].copy_from_slice(&64u64.to_le_bytes());
        bytes[58..60].copy_from_slice(&64u16.to_le_bytes());
        bytes[60..62].copy_from_slice(&2u16.to_le_bytes());
        bytes[68..72].copy_from_slice(&2u32.to_le_bytes());
        bytes[104..108].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[120..128].copy_from_slice(&24u64.to_le_bytes());
        regia_test_utils::write_atomic(&path, &bytes)?;
        assert!(elf_functions(path.to_str().expect("test fixture"), 1024, 64).is_none());
        assert!(elf_functions(path.to_str().expect("test fixture"), 32, 64).is_none());
        assert_eq!(u64_at(&bytes, usize::MAX), None);
        Ok(())
    }
}
