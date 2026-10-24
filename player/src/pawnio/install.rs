//! `wvsensors.exe --install`: copies the reader and its pinned module files into
//! `C:\Program Files\Wonder Vision\`, where only an administrator can change them, so the routine prompt
//! at each start names an administrator-only file (§4.4a). It runs only after the user's explicit ask, as
//! administrator (the reader checks that first).
//!
//! The exit codes are the reader's: 8 the folder isn't administrator-only, 9 another reader is running so
//! the move failed, 10 the module files beside the player are damaged.

use std::path::Path;

use super::ModuleId;
use super::blobs;

/// Exit codes for `--install`, shared with helper.rs (§4.4 step 6).
pub const NOT_ADMIN_ONLY: i32 = 8;
pub const MOVE_FAILED: i32 = 9;
pub const DAMAGED: i32 = 10;

/// Copies `reader_exe` and the pinned modules from `modules_dir` into `target` (`…\Wonder Vision`): the
/// whole `pawnio\` folder bar dotfiles, then the reader itself through a `.new` file and a replace, so a
/// half-written reader is never left. `admin_only` says whether a path is administrator-only; the target,
/// if it was already there, and the finished folder, exe and blobs must all be.
pub fn install(reader_exe: &Path, modules_dir: &Path, target: &Path, admin_only: &dyn Fn(&Path) -> bool) -> Result<(), i32> {
    // the module files beside the player must be the pinned ones
    for module in [ModuleId::AmdFamily17, ModuleId::LpcIo] {
        blobs::read(modules_dir, module).map_err(|_| DAMAGED)?;
    }

    let existed = target.exists();
    let pawnio = target.join("pawnio");
    std::fs::create_dir_all(&pawnio).map_err(|_| NOT_ADMIN_ONLY)?;
    // a target that was already there must be administrator-only before we write into it
    if existed && !admin_only(target) {
        return Err(NOT_ADMIN_ONLY);
    }

    // every file of the module folder, bar dotfiles (.gitattributes), under target\pawnio
    copy_tree(modules_dir, &pawnio).map_err(|_| MOVE_FAILED)?;

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

/// Copies every file under `from` to `to` (making folders), skipping dotfiles.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let (src, dst) = (entry.path(), to.join(&name));
        if src.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            let bytes = std::fs::read(&src)?;
            write_bytes_through(&dst, &bytes)?;
        }
    }
    Ok(())
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
