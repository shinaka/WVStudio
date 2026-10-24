//! The programs built here never load a DLL from their own folder (build.rs, .cargo/config.toml): each
//! imports Windows' own DLLs only, none of the C runtime's, puts off none of them until first use, and has
//! Windows look for them in System32 alone (its load configuration's DependentLoadFlags is 0x800, as
//! `dumpbin /loadconfig` shows it). Read from the programs' PE headers, so no tool is needed.

/// LOAD_LIBRARY_SEARCH_SYSTEM32.
const SYSTEM32_ONLY: u16 = 0x800;

/// What a program's headers say about the DLLs it loads.
struct Imports {
    dlls: Vec<String>,
    delay_loaded: bool,
    dependent_load_flags: Option<u16>,
}

fn u16_at(file: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(file.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(file: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(file.get(at..at + 4)?.try_into().ok()?))
}

/// A 64-bit program's imports, whether it has a delay-load directory, and its load configuration's
/// DependentLoadFlags (None without one). None when it isn't a 64-bit PE.
fn imports(file: &[u8]) -> Option<Imports> {
    let pe = u32_at(file, 0x3C)? as usize;
    if file.get(pe..pe + 4)? != b"PE\0\0" || u16_at(file, pe + 24)? != 0x20B {
        return None;
    }
    let (sections, optional_size) = (u16_at(file, pe + 6)? as usize, u16_at(file, pe + 20)? as usize);
    let (optional, table) = (pe + 24, pe + 24 + optional_size);
    // data directory i's address and size: 1 is the imports, 10 the load configuration, 13 the delay-loaded
    let directory = |i: usize| -> Option<(u32, u32)> {
        if i >= u32_at(file, optional + 108)? as usize {
            return Some((0, 0));
        }
        Some((u32_at(file, optional + 112 + 8 * i)?, u32_at(file, optional + 116 + 8 * i)?))
    };
    // where an address in the loaded program is in the file
    let offset = |rva: u32| -> Option<usize> {
        (0..sections).find_map(|i| {
            let s = table + 40 * i;
            let (size, va, raw_size, raw) = (u32_at(file, s + 8)?, u32_at(file, s + 12)?, u32_at(file, s + 16)?, u32_at(file, s + 20)?);
            (va <= rva && rva < va + size.max(raw_size)).then(|| (rva - va + raw) as usize)
        })
    };
    let mut dlls = Vec::new();
    // one 20-byte descriptor per DLL, its name's address at 12, then an empty one
    let mut at = offset(directory(1)?.0)?;
    while u32_at(file, at + 12)? != 0 {
        let name = offset(u32_at(file, at + 12)?)?;
        let end = file.get(name..)?.iter().position(|&b| b == 0)?;
        dlls.push(String::from_utf8_lossy(&file[name..name + end]).into_owned());
        at += 20;
    }
    let (delay, delay_size) = directory(13)?;
    // IMAGE_LOAD_CONFIG_DIRECTORY64: its size first, DependentLoadFlags at 0x4E
    let dependent_load_flags = match directory(10)?.0 {
        0 => None,
        rva => {
            let config = offset(rva)?;
            (u32_at(file, config)? >= 0x50).then(|| u16_at(file, config + 0x4E)).flatten()
        }
    };
    Some(Imports { dlls, delay_loaded: delay != 0 || delay_size != 0, dependent_load_flags })
}

fn check(exe: &str) {
    let file = std::fs::read(exe).unwrap_or_else(|e| panic!("{exe}: {e}"));
    let found = imports(&file).unwrap_or_else(|| panic!("{exe} isn't a 64-bit program"));
    let crt = |dll: &String| {
        let dll = dll.to_ascii_lowercase();
        ["vcruntime", "msvcp", "ucrtbase", "api-ms-win-crt-"].iter().any(|crt| dll.starts_with(crt))
    };
    assert!(found.dlls.iter().any(|d| d.eq_ignore_ascii_case("kernel32.dll")), "{exe} imports {:?}", found.dlls);
    assert!(!found.dlls.iter().any(crt), "{exe} imports the C runtime: {:?}", found.dlls);
    assert!(!found.delay_loaded, "{exe} delay-loads DLLs");
    assert_eq!(found.dependent_load_flags, Some(SYSTEM32_ONLY), "{exe}");
}

#[test]
fn wvplay_loads_dlls_from_system32_only() {
    check(env!("CARGO_BIN_EXE_wvplay"));
}

#[test]
fn wvplayw_loads_dlls_from_system32_only() {
    check(env!("CARGO_BIN_EXE_wvplayw"));
}

/// wvsensors.exe runs as administrator, so it's held hardest: the System32-only rules of `check`, and on
/// top, every DLL it imports is one of Windows' own (a System32 DLL or an API set), never a third party's.
#[test]
fn wvsensors_loads_dlls_from_system32_only() {
    let exe = env!("CARGO_BIN_EXE_wvsensors");
    check(exe);
    let file = std::fs::read(exe).unwrap();
    let found = imports(&file).unwrap();
    // Windows' own DLLs the reader may import; anything else, or a path, would be a planted dependency
    let system = [
        "kernel32.dll", "advapi32.dll", "ntdll.dll", "bcrypt.dll", "bcryptprimitives.dll", "ws2_32.dll", "ole32.dll",
        "shell32.dll", "user32.dll", "sechost.dll", "rpcrt4.dll", "kernelbase.dll",
    ];
    for dll in &found.dlls {
        let dll = dll.to_ascii_lowercase();
        let ok = dll.starts_with("api-ms-win-") || system.contains(&dll.as_str());
        assert!(ok, "{exe} imports {dll}, which isn't one of Windows' own");
    }
}
