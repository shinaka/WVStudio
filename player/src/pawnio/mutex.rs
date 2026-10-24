//! The named mutexes other monitoring programs take around the same hardware (`Global\Access_PCI`,
//! `Global\Access_ISABUS.HTP.Method`), as `Lock`s.
//!
//! They're the only thing that keeps two programs' sequences on the same ports from interleaving (LHM and
//! HWiNFO take them too; the modules name them in their warnings). Without one, what it guards is skipped,
//! with a note: LHM goes on without it, but that races the programs that share it.

use std::mem::size_of;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, LocalFree, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, MUTEX_MODIFY_STATE, OpenMutexW, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};

use super::{Held, Lock};

/// Anyone may use a mutex made here, as with LHM's, so programs that don't run as administrator can share
/// it too.
const EVERYONE: &str = "D:(A;;GA;;;WD)";

/// What a shared lock guards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bus {
    /// PCI configuration space, where each SMN read of the CPU's temperatures is a write then a read.
    Pci,
    /// The ISA bus: the Super I/O chips' ports, so the fans and the board's temperatures.
    Isa,
}

impl Bus {
    /// The mutex's name.
    pub fn name(self) -> &'static str {
        match self {
            Bus::Pci => r"Global\Access_PCI",
            Bus::Isa => r"Global\Access_ISABUS.HTP.Method",
        }
    }

    /// The note when its mutex can't be had, so what it guards is off.
    pub fn unshared(self) -> String {
        let off = match self {
            Bus::Pci => "CPU temperatures",
            Bus::Isa => "fan and board readings",
        };
        format!("Couldn't share the motherboard sensor lock with other monitoring programs, so {off} are off.")
    }
}

/// The lock other programs share around `bus`, or, when it can be neither made nor opened, the note saying
/// what's off without it.
pub fn shared(bus: Bus) -> Result<GlobalMutex, String> {
    named(bus.name(), bus)
}

fn named(name: &str, bus: Bus) -> Result<GlobalMutex, String> {
    GlobalMutex::open(name).map_err(|_| bus.unshared())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// A named mutex, shared with other programs.
pub struct GlobalMutex {
    handle: HANDLE,
    /// Whether it was made here, rather than opened as another program made it.
    pub created: bool,
}

// a mutex's handle can be used from any thread; what holds it (Held) stays on the thread that took it,
// as only that thread can let go of it
unsafe impl Send for GlobalMutex {}

impl GlobalMutex {
    /// Makes the mutex `name` so anyone may use it, or opens it if it's there already. When it's there
    /// with less than full access for us, opens it with only what a lock needs. The Win32 error when
    /// neither works.
    pub fn open(name: &str) -> Result<GlobalMutex, u32> {
        GlobalMutex::open_as(name, EVERYONE)
    }

    /// `open`, making it with the access `sddl` gives.
    fn open_as(name: &str, sddl: &str) -> Result<GlobalMutex, u32> {
        let name = wide(name);
        unsafe {
            let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(sddl).as_ptr(), SDDL_REVISION_1, &mut descriptor, null_mut()) != 0 {
                let attributes = SECURITY_ATTRIBUTES {
                    nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: descriptor,
                    bInheritHandle: 0,
                };
                let handle = CreateMutexW(&attributes, 0, name.as_ptr());
                let existed = GetLastError() == ERROR_ALREADY_EXISTS;
                LocalFree(descriptor);
                if !handle.is_null() {
                    return Ok(GlobalMutex { handle, created: !existed });
                }
            }
            let handle = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE, 0, name.as_ptr());
            match handle.is_null() {
                true => Err(GetLastError()),
                false => Ok(GlobalMutex { handle, created: false }),
            }
        }
    }
}

impl Lock for GlobalMutex {
    /// Abandoned counts as had: the program that held it ended without letting go, and it's ours now.
    fn acquire(&self, timeout_ms: u32) -> Option<Held<'_>> {
        match unsafe { WaitForSingleObject(self.handle, timeout_ms) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => {
                let handle = self.handle;
                Some(Held::new(move || unsafe {
                    ReleaseMutex(handle);
                }))
            }
            _ => None,
        }
    }
}

impl Drop for GlobalMutex {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::channel;

    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_HANDLE};
    use windows_sys::Win32::System::Threading::CreateEventW;

    /// A name no other program uses, in this session only: never the real ones.
    fn test_name() -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!(r"Local\wvtest-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// Whether another thread, with its own handle, can take the mutex now.
    fn free_elsewhere(name: &str) -> bool {
        let name = name.to_string();
        std::thread::spawn(move || GlobalMutex::open(&name).unwrap().acquire(0).is_some()).join().unwrap()
    }

    /// The first open makes it, the next opens the same one; held by one, it isn't free elsewhere until let go.
    #[test]
    fn create_then_open() {
        let name = test_name();
        let first = GlobalMutex::open(&name).unwrap();
        let second = GlobalMutex::open(&name).unwrap();
        assert!(first.created && !second.created);
        let held = second.acquire(0).unwrap();
        assert!(!free_elsewhere(&name));
        drop(held);
        assert!(free_elsewhere(&name));
        assert!(first.acquire(0).is_some());
    }

    /// One made by a program that gives others only what a lock needs can't be made again, so it's opened
    /// with just that, and works.
    #[test]
    fn opened_with_less_access() {
        let name = test_name();
        let theirs = GlobalMutex::open_as(&name, "D:(A;;0x00100001;;;WD)").unwrap(); // SYNCHRONIZE | MUTEX_MODIFY_STATE
        assert!(theirs.created);
        unsafe {
            assert!(CreateMutexW(std::ptr::null(), 0, wide(&name).as_ptr()).is_null());
            assert_eq!(GetLastError(), ERROR_ACCESS_DENIED);
        }
        let ours = GlobalMutex::open(&name).unwrap();
        assert!(!ours.created);
        let held = ours.acquire(0).unwrap();
        assert!(!free_elsewhere(&name));
        drop(held);
        assert!(free_elsewhere(&name));
    }

    /// Taken, it times out elsewhere; let go, it's had at once.
    #[test]
    fn busy_times_out() {
        let name = test_name();
        let ours = GlobalMutex::open(&name).unwrap();
        let held = ours.acquire(0).unwrap();
        let (tried, results) = channel();
        let (go, wait) = channel();
        let other = name.clone();
        let thread = std::thread::spawn(move || {
            let theirs = GlobalMutex::open(&other).unwrap();
            let started = std::time::Instant::now();
            tried.send((theirs.acquire(20).is_some(), started.elapsed())).unwrap();
            wait.recv().unwrap();
            tried.send((theirs.acquire(2000).is_some(), started.elapsed())).unwrap();
        });
        let (had, waited) = results.recv().unwrap();
        assert!(!had && waited >= std::time::Duration::from_millis(15), "{waited:?}");
        drop(held);
        go.send(()).unwrap();
        assert!(results.recv().unwrap().0);
        thread.join().unwrap();
    }

    /// A mutex whose holder ended without letting go is had: WAIT_ABANDONED counts.
    #[test]
    fn abandoned_counts_as_acquired() {
        let name = test_name();
        let ours = GlobalMutex::open(&name).unwrap();
        let other = name.clone();
        std::thread::spawn(move || {
            let theirs = GlobalMutex::open(&other).unwrap();
            std::mem::forget(theirs.acquire(0).unwrap());
        })
        .join()
        .unwrap();
        let held = ours.acquire(0);
        assert!(held.is_some());
        drop(held);
        assert!(free_elsewhere(&name));
    }

    /// When the name can be neither made nor opened as a mutex (here, something else has it), what the lock
    /// guards is off, with its note.
    #[test]
    fn unshared_is_off_with_a_note() {
        let name = test_name();
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, wide(&name).as_ptr()) };
        assert!(!event.is_null());
        assert_eq!(GlobalMutex::open(&name).err(), Some(ERROR_INVALID_HANDLE));
        assert_eq!(named(&name, Bus::Pci).err().unwrap(),
                   "Couldn't share the motherboard sensor lock with other monitoring programs, so CPU temperatures are off.");
        assert_eq!(named(&name, Bus::Isa).err().unwrap(),
                   "Couldn't share the motherboard sensor lock with other monitoring programs, so fan and board readings are off.");
        unsafe {
            CloseHandle(event);
        }
    }

    /// No test code takes the mutexes other programs share: those names appear only outside tests (fake.rs
    /// is all test code; elsewhere, from a file's first test module on).
    #[test]
    fn tests_never_name_the_shared_mutexes() {
        let shared = [["Access", "_PCI"].concat(), ["Access", "_ISABUS"].concat()];
        let mut checked = 0;
        for (name, text) in super::super::driver::rust_files() {
            let lines: Vec<&str> = text.lines().collect();
            let inline_module = |line: &str| line.trim_end().ends_with('{') && line.trim_start().split(' ').any(|word| word == "mod");
            let tests = match name.ends_with("pawnio/fake.rs") || name.starts_with("tests/") {
                true => 0,
                false => match lines.windows(2).position(|w| w[0].trim() == "#[cfg(test)]" && inline_module(w[1])) {
                    Some(at) => at,
                    None => continue,
                },
            };
            checked += 1;
            for line in &lines[tests..] {
                assert!(!shared.iter().any(|real| line.contains(real.as_str())), "{name}: {line}");
            }
        }
        assert!(checked >= 10, "{checked}");
    }
}
