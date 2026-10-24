//! `wvsensors.exe --install`: copies the reader and its pinned module files into
//! `C:\Program Files\Wonder Vision\`, where only an administrator can change them, so the routine prompt
//! at each start names an administrator-only file (§4.4a). It runs only after the user's explicit ask, as
//! administrator (the reader checks that first).
//!
//! The exit codes are the reader's: 8 the folder isn't administrator-only, 9 another reader is running so
//! the move failed, 10 the module files beside the player are damaged.
//!
//! The module folder beside the player can be changed by anyone (C:\Git grants Authenticated Users
//! Modify), so what's copied from it as administrator is a fixed list of files, each with the SHA-256 this
//! build ships: never a walk of whatever is there, which could hold a junction to a folder only an
//! administrator may read.

use std::path::Path;

use super::ModuleId;
use super::{blobs, sha256};

/// Exit codes for `--install`, shared with helper.rs (§4.4 step 6).
pub const NOT_ADMIN_ONLY: i32 = 8;
pub const MOVE_FAILED: i32 = 9;
pub const DAMAGED: i32 = 10;

/// The module folder's other files, installed beside the two modules (blobs.rs pins those), each with its
/// SHA-256: the licence, the NOTICE and the modules' source (player/pawnio/README.md). A change to one of
/// them changes its pin here too.
const TEXTS: [(&str, &str); 10] = [
    ("COPYING", "1e7e6bae5a5bde32f1ae5a7c37a082d1ab03cf89354f7f936ac40be9e39a6531"),
    ("README.md", "b4899422cfea341cb3184e0b41c4b3c86983a302f17f649c18647ea4accce4e3"),
    ("src\\AMDFamily17.p", "8a1808a2e51e184f96d17362991ae900c65a1a23135a68c256ec6759e8297bb9"),
    ("src\\LpcIO.p", "43f0eae8cbcfbb5d1dd7492c45f343061a8d7c81970af56c15010ece2da1678e"),
    ("src\\include\\core.inc", "3c3fdb0bb54d48792e5fadf4a17607023a35f25f1c006893a114342bac9d6aff"),
    ("src\\include\\extra.inc", "4780c6ba2489264ff5982f2f9a5e2b7f4e8ec8951adfe8ef88e728592f1b2e94"),
    ("src\\include\\native.inc", "b526bf69d66ebe00b22ea8609d9b6c2073ba6eeaf0730a2cf32477d0b64ee395"),
    ("src\\include\\ntstatus.inc", "bcb76abca4bc951f286074b59f64fb950e384a2204d4dfe6d1e0d03502d0bd2a"),
    ("src\\include\\pawnio.inc", "0809f5964d7707272398b212879078f62e08bc579a3def0283a1a059cf9e72a4"),
    ("src\\include\\util.inc", "ab390f9d9e244c2f7a5932f42a7e79194d100dc4a16549dbc31f9cf4932f3027"),
];

/// Copies `reader_exe` and the module folder's files from `modules_dir` into `target` (`…\Wonder Vision`):
/// the two modules and `TEXTS` under `pawnio\`, all read and checked against their pins before anything is
/// written, then the reader itself through a `.new` file and a replace, so a half-written reader is never
/// left. `admin_only` says whether a path is administrator-only; the target, if it was already there, and
/// the finished folder, exe and blobs must all be.
pub fn install(reader_exe: &Path, modules_dir: &Path, target: &Path, admin_only: &dyn Fn(&Path) -> bool) -> Result<(), i32> {
    // everything that's copied, as this build ships it: the pinned modules, then the texts
    let mut files = Vec::new();
    for module in [ModuleId::AmdFamily17, ModuleId::LpcIo] {
        files.push((module.file(), blobs::read(modules_dir, module).map_err(|_| DAMAGED)?));
    }
    for (name, pin) in TEXTS {
        let bytes = std::fs::read(modules_dir.join(name)).map_err(|_| DAMAGED)?;
        if sha256::hex(&bytes) != pin {
            return Err(DAMAGED);
        }
        files.push((name, bytes));
    }

    let existed = target.exists();
    let pawnio = target.join("pawnio");
    std::fs::create_dir_all(&pawnio).map_err(|_| NOT_ADMIN_ONLY)?;
    // a target that was already there must be administrator-only before we write into it
    if existed && !admin_only(target) {
        return Err(NOT_ADMIN_ONLY);
    }

    for (name, bytes) in &files {
        write_bytes_through(&pawnio.join(name), bytes).map_err(|_| MOVE_FAILED)?;
    }

    // the reader through a .new file and a replace, so a running old reader fails the move, not a half-write
    let exe = target.join("wvsensors.exe");
    let new = target.join("wvsensors.exe.new");
    write_through(reader_exe, &new).map_err(|_| MOVE_FAILED)?;
    replace(&new, &exe)?;

    // the finished install must be administrator-only
    let ok = admin_only(target)
        && admin_only(&exe)
        && admin_only(&pawnio)
        && admin_only(&pawnio.join("AMDFamily17.bin"))
        && admin_only(&pawnio.join("LpcIO.bin"));
    if ok { Ok(()) } else { Err(NOT_ADMIN_ONLY) }
}

/// Copies one file to `dst` so it's on disk before the move (flushed, like FILE_FLAG_WRITE_THROUGH).
fn write_through(src: &Path, dst: &Path) -> std::io::Result<()> {
    write_bytes_through(dst, &std::fs::read(src)?)
}

fn write_bytes_through(dst: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(dst)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Moves `new` onto `name`, replacing it, flushed. A running old reader holds `name`, so the move fails and
/// that's exit 9.
#[cfg(not(test))]
fn replace(new: &Path, name: &Path) -> Result<(), i32> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let from: Vec<u16> = new.as_os_str().encode_wide().chain([0]).collect();
    let to: Vec<u16> = name.as_os_str().encode_wide().chain([0]).collect();
    let moved = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
    if moved == 0 { Err(MOVE_FAILED) } else { Ok(()) }
}

/// In tests there's no running reader to hold the file: a plain rename, so the copy and the replace can be
/// checked without Win32.
#[cfg(test)]
fn replace(new: &Path, name: &Path) -> Result<(), i32> {
    std::fs::rename(new, name).map_err(|_| MOVE_FAILED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The repo's module folder, player\pawnio.
    fn vendored() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("pawnio")
    }

    /// A scratch folder, gone when the test is.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("wvplay-install-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A stand-in for the reader, so the install has something to copy as wvsensors.exe.
    fn fake_reader(dir: &Path) -> PathBuf {
        let exe = dir.join("wvsensors.exe");
        std::fs::write(&exe, b"MZ fake reader").unwrap();
        exe
    }

    /// Each text's pin is the repo's file as it is: a changed NOTICE needs its pin changed too. And the
    /// list is the whole module folder bar its .gitattributes and the two modules.
    #[test]
    fn the_texts_are_the_repo_files() {
        for (name, pin) in TEXTS {
            let bytes = std::fs::read(vendored().join(name)).unwrap();
            assert_eq!(sha256::hex(&bytes), pin, "{name}: change its pin in TEXTS");
        }
        fn files(dir: &Path, base: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    files(&path, base, out);
                } else {
                    out.push(path.strip_prefix(base).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let mut found = Vec::new();
        files(&vendored(), &vendored(), &mut found);
        let mut listed: Vec<String> = TEXTS.iter().map(|(name, _)| name.to_string()).collect();
        listed.extend(["AMDFamily17.bin", "LpcIO.bin", ".gitattributes"].map(String::from));
        found.sort();
        listed.sort();
        assert_eq!(found, listed);
    }

    /// A clean install copies the reader and the whole module folder (bar dotfiles), and the finished
    /// folder passes the administrator-only check.
    #[test]
    fn copies_the_reader_and_modules() {
        let scratch = Scratch::new("ok");
        let reader = fake_reader(&scratch.0);
        let target = scratch.0.join("Program Files\\Wonder Vision");
        assert_eq!(install(&reader, &vendored(), &target, &|_| true), Ok(()));
        assert_eq!(std::fs::read(target.join("wvsensors.exe")).unwrap(), b"MZ fake reader");
        assert!(!target.join("wvsensors.exe.new").exists());
        let pawnio = target.join("pawnio");
        assert_eq!(std::fs::read(pawnio.join("AMDFamily17.bin")).unwrap(), std::fs::read(vendored().join("AMDFamily17.bin")).unwrap());
        assert!(pawnio.join("LpcIO.bin").exists() && pawnio.join("COPYING").exists() && pawnio.join("README.md").exists());
        assert!(pawnio.join("src\\AMDFamily17.p").exists() && pawnio.join("src\\include\\pawnio.inc").exists());
        // the repo's .gitattributes is a dotfile: it isn't copied
        assert!(!pawnio.join(".gitattributes").exists());
    }

    /// A copy of the repo's module folder in `dir`, for a test to add to or change.
    fn modules_copy(dir: &Path) -> PathBuf {
        fn copy(from: &Path, to: &Path) {
            std::fs::create_dir_all(to).unwrap();
            for entry in std::fs::read_dir(from).unwrap() {
                let entry = entry.unwrap();
                let (src, dst) = (entry.path(), to.join(entry.file_name()));
                if src.is_dir() { copy(&src, &dst) } else { std::fs::copy(&src, &dst).map(drop).unwrap() }
            }
        }
        let modules = dir.join("modules");
        copy(&vendored(), &modules);
        modules
    }

    /// Only the module folder's own files are copied, as administrator: anything else in it (a file, a
    /// folder, a junction someone made there) stays where it is.
    #[test]
    fn copies_only_the_listed_files() {
        let scratch = Scratch::new("listed");
        let reader = fake_reader(&scratch.0);
        let modules = modules_copy(&scratch.0);
        std::fs::write(modules.join("extra.txt"), b"not ours").unwrap();
        std::fs::create_dir_all(modules.join("x")).unwrap();
        std::fs::write(modules.join("x\\private.txt"), b"someone's").unwrap();
        std::fs::write(modules.join("src\\include\\more.inc"), b"not ours either").unwrap();
        let target = scratch.0.join("Program Files\\Wonder Vision");
        assert_eq!(install(&reader, &modules, &target, &|_| true), Ok(()));
        let pawnio = target.join("pawnio");
        assert!(pawnio.join("COPYING").exists() && pawnio.join("src\\include\\pawnio.inc").exists());
        for stray in ["extra.txt", "x", "src\\include\\more.inc"] {
            assert!(!pawnio.join(stray).exists(), "{stray} was copied");
        }
    }

    /// A licence, NOTICE or source file that isn't the one this build ships is exit 10, before anything is
    /// written: only those exact bytes are ever copied into Program Files.
    #[test]
    fn a_changed_text_is_exit_10() {
        for name in ["COPYING", "README.md", "src\\LpcIO.p", "src\\include\\ntstatus.inc"] {
            let scratch = Scratch::new("text");
            let reader = fake_reader(&scratch.0);
            let modules = modules_copy(&scratch.0);
            let mut bytes = std::fs::read(modules.join(name)).unwrap();
            bytes.extend_from_slice(b"\nchanged");
            std::fs::write(modules.join(name), bytes).unwrap();
            let target = scratch.0.join("Program Files\\Wonder Vision");
            assert_eq!(install(&reader, &modules, &target, &|_| true), Err(DAMAGED), "{name}");
            assert!(!target.exists(), "{name}: something was written");
        }
    }

    /// A module file that isn't the pinned one is exit 10, and nothing is installed.
    #[test]
    fn damaged_modules_are_exit_10() {
        let scratch = Scratch::new("damaged");
        let reader = fake_reader(&scratch.0);
        let modules = scratch.0.join("modules");
        std::fs::create_dir_all(&modules).unwrap();
        for name in ["AMDFamily17.bin", "LpcIO.bin"] {
            std::fs::write(modules.join(name), b"not the real module").unwrap();
        }
        let target = scratch.0.join("Program Files\\Wonder Vision");
        assert_eq!(install(&reader, &modules, &target, &|_| true), Err(DAMAGED));
        assert!(!target.join("wvsensors.exe").exists());
    }

    /// A target folder that was already there and isn't administrator-only is exit 8.
    #[test]
    fn user_writable_target_is_exit_8() {
        let scratch = Scratch::new("writable");
        let reader = fake_reader(&scratch.0);
        let target = scratch.0.join("Program Files\\Wonder Vision");
        std::fs::create_dir_all(&target).unwrap(); // already there, and (per the stub) not admin-only
        assert_eq!(install(&reader, &vendored(), &target, &|_| false), Err(NOT_ADMIN_ONLY));
    }

    /// A finished install that still isn't administrator-only is exit 8.
    #[test]
    fn not_admin_only_afterwards_is_exit_8() {
        let scratch = Scratch::new("after");
        let reader = fake_reader(&scratch.0);
        let target = scratch.0.join("Program Files\\Wonder Vision");
        // the target is new (so the pre-check passes), but the re-check finds it user-writable
        let exe = target.join("wvsensors.exe");
        assert_eq!(install(&reader, &vendored(), &target, &|p| p != exe), Err(NOT_ADMIN_ONLY));
        // it did copy, then refused
        assert!(exe.exists());
    }
}
