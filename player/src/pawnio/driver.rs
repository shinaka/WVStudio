//! The PawnIO device, through raw IOCTLs on `\\?\GLOBALROOT\Device\PawnIO` (never PawnIOLib.dll): opening
//! it, loading one module per handle (LOAD), calling the module's functions (EXECUTE) and asking the
//! driver's version. In tests, opening always fails, so no test reaches the device.
//!
//! The numbers are PawnIO's (its pawnio_um.h, restated here, not copied): three METHOD_BUFFERED IOCTLs on
//! device type 0xA1B2, each answered before DeviceIoControl returns. Errors come back as Win32 codes, which
//! mean different things at the open, at LOAD and at EXECUTE, so each stage has its own reading of them.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE, ERROR_INVALID_DATA,
    ERROR_INVALID_PARAMETER, ERROR_NOT_READY, ERROR_NOT_SUPPORTED, GetLastError, HANDLE, NTE_BAD_SIGNATURE,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use super::{Exec, IoError, ModuleId, OpenError, OpenModule, blobs};

/// An IOCTL code: PawnIO's device type, the function, METHOD_BUFFERED and FILE_ANY_ACCESS.
const fn ioctl(function: u32) -> u32 {
    0xA1B2 << 16 | function << 2
}

/// LOAD: in, a module file as shipped (`[u32 LE signature length][signature][AMX]`); out, nothing.
pub const LOAD: u32 = ioctl(0x821);
/// EXECUTE: in, the function's name (see `encode`), then its input cells; out, its output cells.
pub const EXECUTE: u32 = ioctl(0x841);
/// VERSION: out, exactly 4 bytes. Needs no module.
pub const VERSION: u32 = ioctl(0x861);

/// The bytes the function's name takes at the start of EXECUTE's input, NUL-padded.
const NAME: usize = 32;

/// What an error from opening the device means.
fn open_error(code: u32) -> OpenError {
    match code {
        ERROR_FILE_NOT_FOUND => OpenError::NotInstalled,
        ERROR_ACCESS_DENIED => OpenError::AccessDenied,
        code => OpenError::Other(code),
    }
}

/// What an error from LOAD means.
fn load_error(code: u32) -> OpenError {
    match code {
        // STATUS_INVALID_SIGNATURE, as Windows gives it
        code if code == NTE_BAD_SIGNATURE as u32 => OpenError::BadSignature,
        // the module's main() refused this CPU
        ERROR_NOT_SUPPORTED => OpenError::NotSupported,
        // the driver couldn't load it: it uses something this driver lacks, or it faulted
        ERROR_GEN_FAILURE => OpenError::TooOld,
        ERROR_INVALID_PARAMETER => OpenError::Malformed,
        code => OpenError::Other(code),
    }
}

/// What an error from EXECUTE (or VERSION) means.
fn execute_error(code: u32) -> IoError {
    match code {
        ERROR_ACCESS_DENIED => IoError::AccessDenied,
        ERROR_NOT_READY => IoError::NotReady,
        ERROR_FILE_NOT_FOUND => IoError::NoSuchFunction,
        ERROR_INVALID_PARAMETER => IoError::BadArguments,
        ERROR_GEN_FAILURE => IoError::Fault,
        code => IoError::Other(code),
    }
}

/// EXECUTE's input: the function's name, NUL-padded to 32 bytes, then each input cell as a little-endian
/// u64. None for a name the driver refuses: one that doesn't start `ioctl_` (only those can be called from
/// outside the module), or is over 31 characters (the NUL must fit), or isn't plain ASCII.
fn encode(function: &str, input: &[u64]) -> Option<Vec<u8>> {
    let name = function.as_bytes();
    if !function.starts_with("ioctl_") || name.len() >= NAME || !name.iter().all(u8::is_ascii_graphic) {
        return None;
    }
    let mut bytes = vec![0u8; NAME];
    bytes[..name.len()].copy_from_slice(name);
    bytes.extend(input.iter().flat_map(|cell| cell.to_le_bytes()));
    Some(bytes)
}

/// EXECUTE's output, as cells.
fn decode(bytes: &[u8], out: &mut [u64]) {
    for (cell, bytes) in out.iter_mut().zip(bytes.chunks_exact(8)) {
        *cell = u64::from_le_bytes(bytes.try_into().unwrap());
    }
}

/// A handle on the PawnIO device, with no module on it yet.
pub struct Device(HANDLE);

// the driver takes a handle's calls from any thread, one at a time
unsafe impl Send for Device {}

impl Device {
    /// Opens the device. Only an administrator may (`AccessDenied` otherwise); `NotInstalled` when PawnIO
    /// isn't installed or its driver isn't running.
    #[cfg(not(test))]
    pub fn open() -> Result<Device, OpenError> {
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        // the device has no DOS name on PawnIO 2.1.0, so it's reached through GLOBALROOT, as LHM does
        let path: Vec<u16> = r"\\?\GLOBALROOT\Device\PawnIO".encode_utf16().chain([0]).collect();
        // not overlapped: the driver finishes every call before it returns
        let handle = unsafe {
            CreateFileW(path.as_ptr(), GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, null(),
                        OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, null_mut())
        };
        match handle == INVALID_HANDLE_VALUE {
            true => Err(open_error(unsafe { GetLastError() })),
            false => Ok(Device(handle)),
        }
    }

    /// Tests never reach the device: opening it always fails.
    #[cfg(test)]
    pub fn open() -> Result<Device, OpenError> {
        Err(OpenError::Other(0))
    }

    /// The driver's version, (major << 16) | (minor << 8) | patch: 0x020100 is 2.1.0.
    pub fn version(&self) -> Result<u32, IoError> {
        let mut out = [0u8; 4];
        match self.call(VERSION, &[], &mut out).map_err(execute_error)? {
            4 => Ok(u32::from_le_bytes(out)),
            _ => Err(IoError::Other(ERROR_INVALID_DATA)),
        }
    }

    /// Loads a module (its whole file, as `blobs::read` gives it) on this handle, which then holds it:
    /// dropping the module closes the handle, and the driver unloads the module, also when the process
    /// ends however it ends. A handle holds one module only.
    pub fn load(self, module: &[u8]) -> Result<Module, OpenError> {
        self.call(LOAD, module, &mut []).map_err(load_error)?;
        Ok(Module(self))
    }

    /// One IOCTL: the number of bytes it answered with, or its Win32 error.
    fn call(&self, code: u32, input: &[u8], out: &mut [u8]) -> Result<usize, u32> {
        let input_at = if input.is_empty() { null() } else { input.as_ptr() as *const c_void };
        let out_at = if out.is_empty() { null_mut() } else { out.as_mut_ptr() as *mut c_void };
        let mut answered = 0u32;
        let done = unsafe {
            DeviceIoControl(self.0, code, input_at, input.len() as u32, out_at, out.len() as u32, &mut answered, null_mut())
        };
        match done {
            0 => Err(unsafe { GetLastError() }),
            _ => Ok(answered as usize),
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// A module loaded on a device handle of its own.
pub struct Module(Device);

impl Exec for Module {
    fn execute(&self, function: &str, input: &[u64], out: &mut [u64]) -> Result<(), IoError> {
        let input = encode(function, input).ok_or(IoError::BadArguments)?;
        let mut bytes = vec![0u8; out.len() * 8];
        let answered = self.0.call(EXECUTE, &input, &mut bytes).map_err(execute_error)?;
        // PawnIO always answers with the whole output; anything short would leave cells unread
        if answered != bytes.len() {
            return Err(IoError::Other(ERROR_INVALID_DATA));
        }
        decode(&bytes, out);
        Ok(())
    }
}

/// Opens `module` from the module folder `dir` (`blobs::find_dir`): its pinned file, loaded on a device
/// handle of its own. A file that isn't the pinned one never reaches the device.
pub fn open_module(dir: &Path, module: ModuleId) -> Result<Box<dyn Exec>, OpenError> {
    let file = blobs::read(dir, module)?;
    Ok(Box::new(Device::open()?.load(&file)?))
}

/// How a reader opens the modules in `dir`, for real.
pub fn opener(dir: PathBuf) -> OpenModule {
    Box::new(move |module| open_module(&dir, module))
}

/// Tests: every Rust file of the player (src and tests), by its path from player\, with its text, so tests
/// can check what test code names.
#[cfg(test)]
pub(crate) fn rust_files() -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, files: &mut Vec<(String, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, root, files);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let name = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                files.push((name, std::fs::read_to_string(&path).unwrap()));
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in ["src", "tests"] {
        walk(&root.join(dir), root, &mut files);
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three IOCTLs, as PawnIO numbers them.
    #[test]
    fn ioctl_codes() {
        assert_eq!(LOAD, 0xA1B2_2084);
        assert_eq!(EXECUTE, 0xA1B2_2104);
        assert_eq!(VERSION, 0xA1B2_2184);
    }

    /// A function's name takes 32 bytes, NUL-padded, so 31 characters at most; it must start `ioctl_`;
    /// the cells follow as little-endian u64s.
    #[test]
    fn names() {
        let bytes = encode("ioctl_read_smn", &[0x0005_9800]).unwrap();
        assert_eq!(bytes.len(), 40);
        assert_eq!(&bytes[..14], b"ioctl_read_smn");
        assert!(bytes[14..32].iter().all(|&b| b == 0));
        assert_eq!(bytes[32..], [0x00, 0x98, 0x05, 0, 0, 0, 0, 0]);
        let bytes = encode("ioctl_pio_outb", &[0x2E, 0x87]).unwrap();
        assert_eq!((bytes.len(), &bytes[32..40], &bytes[40..]), (48, &[0x2E, 0, 0, 0, 0, 0, 0, 0][..], &[0x87, 0, 0, 0, 0, 0, 0, 0][..]));
        assert_eq!(encode("ioctl_find_bars", &[]).unwrap().len(), 32);
        let longest = format!("ioctl_{}", "x".repeat(25));
        assert_eq!(longest.len(), 31);
        assert_eq!(&encode(&longest, &[]).unwrap()[..32], format!("{longest}\0").as_bytes());
        for refused in [format!("{longest}x"), "read_smn".into(), "main".into(), "Ioctl_read_smn".into(), "ioctl_read\0smn".into(),
                        "ioctl_read smn".into(), "ioctl_lecture_°".into()] {
            assert_eq!(encode(&refused, &[1]), None, "{refused:?}");
        }
    }

    /// Output cells are read as little-endian u64s, as many as were asked for.
    #[test]
    fn cells() {
        let mut out = [0u64; 2];
        decode(&[0x15, 0x0B, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], &mut out);
        assert_eq!(out, [0xB15, u64::MAX]);
    }

    /// Each stage's Win32 codes: 2 and 5 mean one thing at the open and another at EXECUTE.
    #[test]
    fn errors_by_stage() {
        for (code, open) in [(2, OpenError::NotInstalled), (5, OpenError::AccessDenied), (3, OpenError::Other(3)), (87, OpenError::Other(87))] {
            assert_eq!(open_error(code), open, "open {code}");
        }
        for (code, load) in [(0x8009_0006, OpenError::BadSignature), (50, OpenError::NotSupported), (31, OpenError::TooOld),
                             (87, OpenError::Malformed), (2, OpenError::Other(2)), (5, OpenError::Other(5)), (1247, OpenError::Other(1247))] {
            assert_eq!(load_error(code), load, "load {code}");
        }
        for (code, execute) in [(5, IoError::AccessDenied), (21, IoError::NotReady), (2, IoError::NoSuchFunction), (87, IoError::BadArguments),
                                (31, IoError::Fault), (50, IoError::Other(50)), (0x8009_0006, IoError::Other(0x8009_0006))] {
            assert_eq!(execute_error(code), execute, "execute {code}");
        }
    }

    /// In tests the device never opens: opening a module reads and checks its file, then stops there.
    #[test]
    fn tests_never_open_the_device() {
        assert_eq!(Device::open().err(), Some(OpenError::Other(0)));
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("pawnio");
        for module in [ModuleId::AmdFamily17, ModuleId::LpcIo] {
            assert_eq!(open_module(&repo, module).err(), Some(OpenError::Other(0)), "{module:?}");
            assert_eq!(opener(repo.clone())(module).err(), Some(OpenError::Other(0)), "{module:?}");
            // without its pinned file, it doesn't get that far
            assert_eq!(open_module(&repo.join("missing"), module).err(), Some(OpenError::Damaged), "{module:?}");
        }
    }

    /// The device's path is in the program once, in `Device::open`, which tests don't build: no test can
    /// open the device.
    #[test]
    fn device_path_only_outside_tests() {
        let device = ["GLOBAL", "ROOT"].concat();
        let mut found = Vec::new();
        for (name, text) in rust_files() {
            let lines: Vec<&str> = text.lines().collect();
            for (at, line) in lines.iter().enumerate() {
                if line.contains(&device) && !line.trim_start().starts_with("//") {
                    let cfg = lines[..at].iter().rev().find(|l| l.trim_start().starts_with("#[cfg("));
                    found.push((name.clone(), cfg.map(|l| l.trim().to_string())));
                }
            }
        }
        assert_eq!(found, [("src/pawnio/driver.rs".to_string(), Some("#[cfg(not(test))]".to_string()))]);
    }
}
