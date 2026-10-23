//! The module files: finding the module folder from the player's own path (`<exe dir>\pawnio\`, else the
//! repo's `player\pawnio\` when it runs from `player\target\<profile>\`), never from the working folder,
//! and reading a module, refusing one whose SHA-256 isn't the one pinned here.
//!
//! The files are PawnIO.Modules release 0.2.11, unmodified and LGPL (player/pawnio/README.md), so they're
//! read at run time, never built into the program. Nothing is ever downloaded.

use std::path::{Path, PathBuf};

use super::{ModuleId, OpenError, sha256};

/// The note when the module files aren't the pinned ones (`OpenError::Damaged`).
pub const DAMAGED: &str =
    "The PawnIO module files beside the player are damaged or from another version: rebuild or reinstall the player.";

/// Every module this player loads, each of which the module folder must hold.
const MODULES: [ModuleId; 2] = [ModuleId::AmdFamily17, ModuleId::LpcIo];

/// The module's SHA-256. The driver loads any module PawnIO signed, of any version; the player loads only
/// these, the ones its code was written against.
pub fn pin(module: ModuleId) -> &'static str {
    match module {
        ModuleId::AmdFamily17 => "dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91",
        ModuleId::LpcIo => "b3896a1cab0d808fca31fe2ebcae045d59dac690da87b17c858bb8da357eb45e",
    }
}

/// The module folder for the program at `exe`: the first of `<exe dir>\pawnio\` (installed, or unzipped
/// from a release) and `<exe dir>\..\..\pawnio\` (the repo's, from `player\target\<profile>\`) that holds
/// every module with its pinned hash. None for an `exe` that isn't a whole path, so the working folder
/// never counts.
pub fn find_dir(exe: &Path) -> Option<PathBuf> {
    if !exe.is_absolute() {
        return None;
    }
    let dir = exe.parent()?;
    let repo = dir.parent().and_then(Path::parent).map(|player| player.join("pawnio"));
    std::iter::once(dir.join("pawnio"))
        .chain(repo)
        .find(|folder| MODULES.iter().all(|&module| read(folder, module).is_ok()))
}

/// The module's file from the folder `dir`, as it's sent to the driver: `Damaged` when it's missing or
/// isn't the one pinned.
pub fn read(dir: &Path, module: ModuleId) -> Result<Vec<u8>, OpenError> {
    let file = std::fs::read(dir.join(module.file())).map_err(|_| OpenError::Damaged)?;
    match sha256::hex(&file) == pin(module) {
        true => Ok(file),
        false => Err(OpenError::Damaged),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The repo's module folder, player\pawnio.
    fn vendored() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("pawnio")
    }

    /// A folder of module folders, gone when the test is.
    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Folder {
            let dir = std::env::temp_dir().join(format!("wvplay-pawnio-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Folder(dir)
        }

        /// A module folder at `path` in it, with a copy of each module the repo has; `change` then has
        /// its way with the copies.
        fn modules(&self, path: &str, change: impl Fn(ModuleId, &mut Vec<u8>)) -> PathBuf {
            let dir = self.0.join(path);
            std::fs::create_dir_all(&dir).unwrap();
            for module in MODULES {
                let mut file = std::fs::read(vendored().join(module.file())).unwrap();
                change(module, &mut file);
                std::fs::write(dir.join(module.file()), file).unwrap();
            }
            dir
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn unchanged(_: ModuleId, _: &mut Vec<u8>) {}

    /// The repo's modules are the pinned ones, and the repo's programs find them: player\pawnio holds
    /// release 0.2.11's AMDFamily17.bin and LpcIO.bin, unmodified.
    #[test]
    fn pins_match_the_repo() {
        assert_eq!(pin(ModuleId::AmdFamily17), "dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91");
        assert_eq!(pin(ModuleId::LpcIo), "b3896a1cab0d808fca31fe2ebcae045d59dac690da87b17c858bb8da357eb45e");
        for module in MODULES {
            let file = read(&vendored(), module).unwrap_or_else(|e| panic!("player\\pawnio\\{}: {e:?}", module.file()));
            assert_eq!(sha256::hex(&file), pin(module));
        }
        for target in ["target", "target-agent"] {
            let exe = Path::new(env!("CARGO_MANIFEST_DIR")).join(target).join("release").join("wvplay.exe");
            assert_eq!(find_dir(&exe), Some(vendored()), "{}", exe.display());
        }
    }

    /// A module file that's missing, changed or cut short is refused.
    #[test]
    fn read_refuses_other_files() {
        let root = Folder::new("read");
        let good = root.modules("good", unchanged);
        assert_eq!(read(&good, ModuleId::LpcIo).unwrap(), std::fs::read(vendored().join("LpcIO.bin")).unwrap());
        assert_eq!(read(&root.0, ModuleId::AmdFamily17), Err(OpenError::Damaged));
        let changed = root.modules("changed", |_, file| file[600] ^= 1);
        let short = root.modules("short", |_, file| file.truncate(file.len() - 1));
        // each pinned to its own file: one module's file under the other's name is refused too
        let swapped = root.modules("swapped", |module, file| {
            *file = std::fs::read(vendored().join(if module == ModuleId::LpcIo { "AMDFamily17.bin" } else { "LpcIO.bin" })).unwrap()
        });
        for dir in [&changed, &short, &swapped] {
            for module in MODULES {
                assert_eq!(read(dir, module), Err(OpenError::Damaged), "{} {module:?}", dir.display());
            }
        }
    }

    /// Both layouts are found; a folder with a missing or changed file is passed over for the next.
    #[test]
    fn find_dir_layouts() {
        let root = Folder::new("layouts");
        // installed, or a release zip: the modules beside the program
        let beside = root.modules("installed\\pawnio", unchanged);
        assert_eq!(find_dir(&root.0.join("installed\\wvplayw.exe")), Some(beside));
        // the repo: the program in player\target\release, the modules in player\pawnio
        let repo = root.modules("repo\\player\\pawnio", unchanged);
        assert_eq!(find_dir(&root.0.join("repo\\player\\target\\release\\wvplay.exe")), Some(repo.clone()));
        // beside it first, when that's whole
        let first = root.modules("repo\\player\\target\\release\\pawnio", unchanged);
        assert_eq!(find_dir(&root.0.join("repo\\player\\target\\release\\wvplay.exe")), Some(first.clone()));
        // a changed file there falls through to the repo's
        root.modules("repo\\player\\target\\release\\pawnio", |module, file| {
            if module == ModuleId::LpcIo {
                file[0] ^= 1
            }
        });
        assert_eq!(find_dir(&root.0.join("repo\\player\\target\\release\\wvplay.exe")), Some(repo.clone()));
        // and so does a missing one
        root.modules("repo\\player\\target\\release\\pawnio", unchanged);
        std::fs::remove_file(first.join("AMDFamily17.bin")).unwrap();
        assert_eq!(find_dir(&root.0.join("repo\\player\\target\\release\\wvplay.exe")), Some(repo.clone()));
        // with neither whole, nothing
        std::fs::remove_file(repo.join("LpcIO.bin")).unwrap();
        assert_eq!(find_dir(&root.0.join("repo\\player\\target\\release\\wvplay.exe")), None);
        // no folder above the program's own: only beside it
        assert_eq!(find_dir(Path::new("C:\\wvplay.exe")), None);
    }

    /// A program path that isn't whole is never looked up from the working folder, though the working
    /// folder here (player\, where cargo runs tests) has a whole pawnio\ folder in it.
    #[test]
    fn never_the_working_folder() {
        let working = std::env::current_dir().unwrap();
        assert_eq!(find_dir(&working.join("wvplay.exe")), Some(working.join("pawnio")));
        for exe in ["wvplay.exe", "target\\release\\wvplay.exe", "x\\target\\release\\wvplay.exe", "\\wvplay.exe", "C:wvplay.exe"] {
            assert_eq!(find_dir(Path::new(exe)), None, "{exe}");
        }
    }
}
