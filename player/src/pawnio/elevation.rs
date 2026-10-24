//! The Windows side of asking for administrator rights: whether this process is elevated and whether the
//! account could be (its token), whether PawnIO is installed and its driver running, where Program Files
//! is (from the registry, never the environment), whether a file is administrator-only (its owner and
//! DACL), and raising the prompt that starts the reader. The probe shares the token, service and installed
//! checks.
//!
//! Only `RealEnv` (the player's real one) raises a prompt or starts a process, and it's built only outside
//! tests, so no test reaches `ShellExecuteExW` or `CreateProcessW`.

use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HLOCAL, LocalFree};

use super::control::Elevation;
use super::labels::win;
use super::sha256;

/// SYSTEM, Administrators and TrustedInstaller (resolved on this PC): the only owners, and the only other
/// SIDs that may be granted write, for a folder or file to count as administrator-only.
const TRUSTED_SIDS: [&str; 3] = [
    "S-1-5-18",
    "S-1-5-32-544",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

/// Whether this process runs as administrator: its token is elevated (R-E §1.1, an exact 4-byte buffer).
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TokenElevation};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        // TOKEN_ELEVATION is one u32, TokenIsElevated
        let mut elevated = 0u32;
        let mut size = 0u32;
        let asked = GetTokenInformation(token, TokenElevation, (&raw mut elevated).cast(), 4, &mut size);
        CloseHandle(token);
        asked != 0 && elevated != 0
    }
}

/// This account's rights (R-E §1.1): Full when elevated, Limited for a split-token administrator (whom a
/// prompt can elevate), Default for a standard user or UAC off (whom it can't).
pub fn elevation_type() -> Elevation {
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_QUERY, TokenElevationType, TokenElevationTypeFull, TokenElevationTypeLimited,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Elevation::Default;
        }
        // TOKEN_ELEVATION_TYPE is one i32
        let mut kind = 0i32;
        let mut size = 0u32;
        let asked = GetTokenInformation(token, TokenElevationType, (&raw mut kind).cast(), 4, &mut size);
        CloseHandle(token);
        if asked == 0 {
            Elevation::Default
        } else if kind == TokenElevationTypeFull {
            Elevation::Full
        } else if kind == TokenElevationTypeLimited {
            Elevation::Limited
        } else {
            Elevation::Default
        }
    }
}

/// PawnIO's installed version, from its uninstall entry's DisplayVersion (the 64-bit view, as a 64-bit
/// program reads it), or None when it isn't installed.
pub fn installed_version() -> Option<String> {
    win::machine_string(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO", "DisplayVersion")
}

/// Whether the PawnIO driver's service is running.
pub fn service_running() -> bool {
    service_state("PawnIO") == "running"
}

/// A service's state: "running", "stopped"..., "not installed", or the error asking gave. Read-only, and
/// it works without administrator rights.
pub fn service_state(name: &str) -> String {
    use windows_sys::Win32::Foundation::{ERROR_SERVICE_DOES_NOT_EXIST, GetLastError};
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS,
        SERVICE_STATUS,
    };
    let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
    unsafe {
        let manager = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if manager.is_null() {
            return format!("error {}", GetLastError());
        }
        let service = OpenServiceW(manager, name.as_ptr(), SERVICE_QUERY_STATUS);
        let state = if service.is_null() {
            match GetLastError() {
                ERROR_SERVICE_DOES_NOT_EXIST => "not installed".to_string(),
                code => format!("error {code}"),
            }
        } else {
            let mut status: SERVICE_STATUS = std::mem::zeroed();
            let state = match QueryServiceStatus(service, &mut status) {
                0 => format!("error {}", GetLastError()),
                _ => match status.dwCurrentState {
                    1 => "stopped".to_string(),
                    2 => "start pending".to_string(),
                    3 => "stop pending".to_string(),
                    4 => "running".to_string(),
                    5 => "continue pending".to_string(),
                    6 => "pause pending".to_string(),
                    7 => "paused".to_string(),
                    other => format!("state {other}"),
                },
            };
            CloseServiceHandle(service);
            state
        };
        CloseServiceHandle(manager);
        state
    }
}

/// `C:\Program Files` as the registry has it (the 64-bit view), never from the environment, which the user
/// can change (Appendix C).
pub fn program_files_dir() -> Option<PathBuf> {
    win::machine_string(r"SOFTWARE\Microsoft\Windows\CurrentVersion", "ProgramFilesDir").map(PathBuf::from)
}

/// A file's SHA-256, or None when it isn't there.
pub fn file_sha256(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|bytes| sha256::hex(&bytes))
}

/// Monotonic time since the first call, for the one-prompt-per-10 s limit.
pub fn now() -> Duration {
    static BASE: OnceLock<Instant> = OnceLock::new();
    BASE.get_or_init(Instant::now).elapsed()
}

/// The reader's pipe, named from the player's own process id (`\\.\pipe\WonderVision.sensors.<pid>`).
pub fn pipe_name(player_pid: u32) -> String {
    format!(r"\\.\pipe\WonderVision.sensors.{player_pid}")
}

/// Whether `path` can be changed only by an administrator: it exists, its owner is SYSTEM, Administrators
/// or TrustedInstaller, its DACL isn't NULL, and no ACE that applies to the object itself grants write to
/// any other SID (§4.2). A pure read of the owner and DACL, so a Medium or a High process gets the same
/// answer. Anything it can't read is not administrator-only.
pub fn admin_only(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    unsafe {
        let mut owner: PSID = null_mut();
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let got = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        );
        if got != 0 {
            return false;
        }
        let ok = trusted(owner) && dacl_admin_only(dacl);
        LocalFree(descriptor as HLOCAL);
        ok
    }
}

/// Whether a DACL grants write to no SID but the trusted ones, failing closed: a NULL DACL grants everyone
/// everything; an inherit-only ACE doesn't apply to the object itself, and a deny or audit ACE grants
/// nothing, so those are skipped. An allow ACE (plain, or a conditional "callback" one, whose SID sits
/// where the plain one's does) that grants write must name a trusted SID. Any other ACE that grants write
/// (an object or compound ACE, or a type this doesn't know) counts as granting it to someone else.
///
/// # Safety
/// `dacl` is null or points to a valid ACL.
unsafe fn dacl_admin_only(dacl: *const windows_sys::Win32::Security::ACL) -> bool {
    use windows_sys::Win32::Foundation::{GENERIC_ALL, GENERIC_WRITE};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL_SIZE_INFORMATION, AclSizeInformation, GetAce, GetAclInformation,
        INHERIT_ONLY_ACE, PSID,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, WRITE_DAC,
        WRITE_OWNER,
    };

    // ACE types (winnt.h; not pulling in SystemServices for a few values): the two allow ACEs laid out as
    // header, mask, SID; and those that grant nothing: denies (1, 6, 10, 12) and the system ones (audit,
    // alarm, label and the rest, 2, 3, 7, 8 and 13 to 21), which a DACL shouldn't hold anyway
    const ALLOWED: u8 = 0;
    const ALLOWED_CALLBACK: u8 = 9;
    const GRANTS_NOTHING: [u8; 17] = [1, 6, 10, 12, 2, 3, 7, 8, 13, 14, 15, 16, 17, 18, 19, 20, 21];

    const WRITE: u32 = FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | DELETE
        | FILE_DELETE_CHILD
        | WRITE_DAC
        | WRITE_OWNER
        | GENERIC_WRITE
        | GENERIC_ALL;

    if dacl.is_null() {
        return false;
    }
    unsafe {
        let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
        if GetAclInformation(dacl, (&raw mut info).cast(), size_of::<ACL_SIZE_INFORMATION>() as u32, AclSizeInformation) == 0 {
            return false;
        }
        for i in 0..info.AceCount {
            let mut ace: *mut core::ffi::c_void = null_mut();
            if GetAce(dacl, i, &mut ace) == 0 {
                return false;
            }
            let header = ace as *const ACE_HEADER;
            let kind = (*header).AceType;
            if (*header).AceFlags as u32 & INHERIT_ONLY_ACE != 0 || GRANTS_NOTHING.contains(&kind) {
                continue;
            }
            // every ACE has its mask right after the header
            let allowed = ace as *const ACCESS_ALLOWED_ACE;
            if (*allowed).Mask & WRITE == 0 {
                continue;
            }
            if kind != ALLOWED && kind != ALLOWED_CALLBACK {
                return false;
            }
            let sid = std::ptr::addr_of!((*allowed).SidStart) as PSID;
            if !trusted(sid) {
                return false;
            }
        }
        true
    }
}

/// Windows' sentence for a Win32 error code, without its final period ("The system cannot find the file
/// specified"), or "error N" when Windows has none.
pub fn system_message(code: u32) -> String {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW,
    };
    let mut buffer = [0u16; 512];
    let length = unsafe {
        FormatMessageW(FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS, std::ptr::null(), code, 0,
                       buffer.as_mut_ptr(), buffer.len() as u32, std::ptr::null())
    };
    let text = String::from_utf16_lossy(&buffer[..length as usize]);
    let text = text.trim_end().trim_end_matches('.').trim_end();
    if text.is_empty() { format!("error {code}") } else { text.to_string() }
}

/// Whether a SID is one of the trusted ones (as a string).
fn trusted(sid: windows_sys::Win32::Security::PSID) -> bool {
    sid_string(sid).is_some_and(|s| TRUSTED_SIDS.contains(&s.as_str()))
}

/// This process's user SID as a string ("S-1-5-21-…"), for the reader's pipe DACL, or None.
pub fn user_sid_string() -> Option<String> {
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut size = 0u32;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut size);
        let mut buffer = vec![0u8; size as usize];
        let got = GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), size, &mut size);
        CloseHandle(token);
        if got == 0 {
            return None;
        }
        let user = buffer.as_ptr() as *const TOKEN_USER;
        sid_string((*user).User.Sid)
    }
}

/// A SID as its string form ("S-1-5-18"), or None.
pub(crate) fn sid_string(sid: windows_sys::Win32::Security::PSID) -> Option<String> {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    if sid.is_null() {
        return None;
    }
    unsafe {
        let mut out: windows_sys::core::PWSTR = null_mut();
        if ConvertSidToStringSidW(sid, &mut out) == 0 {
            return None;
        }
        let len = (0..).take_while(|&i| *out.add(i) != 0).count();
        let string = String::from_utf16_lossy(std::slice::from_raw_parts(out, len));
        LocalFree(out as HLOCAL);
        Some(string)
    }
}

#[cfg(not(test))]
pub use real::RealEnv;

/// The real `Env`: the only one that raises a prompt or starts a process. Built only outside tests.
#[cfg(not(test))]
mod real {
    use std::path::Path;
    use std::ptr::null;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_CANCELLED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetExitCodeProcess, INFINITE, WaitForSingleObject};

    use super::super::client;
    use super::super::control::{ConnectError, Elevation, Env, Launched, Pipe};
    use super::{admin_only, elevation_type, file_sha256, installed_version, is_elevated, now, pipe_name, program_files_dir,
                service_running};

    /// A process handle that closes when dropped.
    struct OwnedProcess(HANDLE);
    unsafe impl Send for OwnedProcess {}
    impl Drop for OwnedProcess {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// The reader's process, once started.
    struct RealLaunched {
        process: OwnedProcess,
        pid: u32,
        name: String,
    }

    impl Launched for RealLaunched {
        fn pid(&self) -> u32 {
            self.pid
        }
        fn pipe_name(&self) -> String {
            self.name.clone()
        }
        fn exited(&self) -> Option<i32> {
            unsafe {
                if WaitForSingleObject(self.process.0, 0) != WAIT_OBJECT_0 {
                    return None;
                }
                let mut code = 0u32;
                GetExitCodeProcess(self.process.0, &mut code);
                Some(code as i32)
            }
        }
        fn wait(self: Box<Self>) -> i32 {
            unsafe {
                WaitForSingleObject(self.process.0, INFINITE);
                let mut code = 0u32;
                GetExitCodeProcess(self.process.0, &mut code);
                code as i32
            }
        }
    }

    #[derive(Default)]
    pub struct RealEnv;

    impl Env for RealEnv {
        fn is_elevated(&self) -> bool {
            is_elevated()
        }
        fn elevation(&self) -> Elevation {
            elevation_type()
        }
        fn installed_version(&self) -> Option<String> {
            installed_version()
        }
        fn service_running(&self) -> bool {
            service_running()
        }
        fn program_files(&self) -> Option<std::path::PathBuf> {
            program_files_dir()
        }
        fn build_reader(&self) -> Option<std::path::PathBuf> {
            std::env::current_exe().ok().map(|exe| exe.with_file_name("wvsensors.exe"))
        }
        fn sha256(&self, path: &Path) -> Option<String> {
            file_sha256(path)
        }
        fn admin_only(&self, path: &Path) -> bool {
            admin_only(path)
        }
        fn now(&self) -> Duration {
            now()
        }

        fn launch(&self, target: &Path, install: bool, elevated: bool) -> Result<Box<dyn Launched>, u32> {
            let player = unsafe { GetCurrentProcessId() };
            let name = pipe_name(player);
            let args = format!("{}--parent {player}", if install { "--install " } else { "" });
            let process = if elevated { create_process(target, &args)? } else { run_as(target, &args)? };
            let pid = unsafe { windows_sys::Win32::System::Threading::GetProcessId(process.0) };
            Ok(Box::new(RealLaunched { process, pid, name }))
        }

        fn connect(&self, launched: &dyn Launched, timeout: Duration) -> Result<Box<dyn Pipe>, ConnectError> {
            client::connect_pipe(&launched.pipe_name(), launched.pid(), timeout, &|| launched.exited())
                .map(|c| Box::new(c) as Box<dyn Pipe>)
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// Starts the reader elevated through the consent prompt. The call blocks while the prompt is up, so it
    /// runs on its own thread. `Err(1223)` is the user declining.
    fn run_as(target: &Path, args: &str) -> Result<OwnedProcess, u32> {
        use std::os::windows::ffi::OsStrExt;

        use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
        use windows_sys::Win32::UI::Shell::{
            SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

        let file: Vec<u16> = target.as_os_str().encode_wide().chain([0]).collect();
        let dir: Vec<u16> = target.parent().map(|p| p.as_os_str().encode_wide().chain([0]).collect()).unwrap_or_else(|| vec![0]);
        let verb = wide("runas");
        let params = wide(args);
        unsafe {
            CoInitializeEx(null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
            info.lpVerb = verb.as_ptr();
            info.lpFile = file.as_ptr();
            info.lpParameters = params.as_ptr();
            info.lpDirectory = dir.as_ptr();
            info.nShow = SW_HIDE;
            if ShellExecuteExW(&mut info) == 0 {
                return Err(GetLastError());
            }
            if info.hProcess.is_null() {
                return Err(ERROR_CANCELLED);
            }
            Ok(OwnedProcess(info.hProcess))
        }
    }

    /// Starts the reader with no prompt, when the player is already elevated: the child runs in this
    /// process's security context (CreateProcessW).
    fn create_process(target: &Path, args: &str) -> Result<OwnedProcess, u32> {
        use std::os::windows::ffi::OsStrExt;

        use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW};

        let file: Vec<u16> = target.as_os_str().encode_wide().chain([0]).collect();
        let dir: Vec<u16> = target.parent().map(|p| p.as_os_str().encode_wide().chain([0]).collect()).unwrap_or_else(|| vec![0]);
        // lpCommandLine is argv[0] (the quoted exe) then the arguments, and must be writable
        let mut command: Vec<u16> = wide(&format!("\"{}\" {args}", target.display()));
        unsafe {
            let mut startup: STARTUPINFOW = std::mem::zeroed();
            startup.cb = size_of::<STARTUPINFOW>() as u32;
            let mut info: PROCESS_INFORMATION = std::mem::zeroed();
            let made = CreateProcessW(file.as_ptr(), command.as_mut_ptr(), null(), null(), 0, CREATE_NO_WINDOW, null(),
                                      dir.as_ptr(), &startup, &mut info);
            if made == 0 {
                return Err(GetLastError());
            }
            CloseHandle(info.hThread);
            Ok(OwnedProcess(info.hProcess))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Administrator-only: Windows' own files and Program Files, where only an administrator can write; not
    /// a fresh folder of our own, which we can.
    #[test]
    fn admin_only_tells_windows_from_ours() {
        assert!(admin_only(Path::new(r"C:\Windows\System32\kernel32.dll")));
        assert!(admin_only(Path::new(r"C:\Program Files")));
        let ours = std::env::temp_dir().join(format!("wvplay-adminonly-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ours);
        std::fs::create_dir_all(&ours).unwrap();
        assert!(!admin_only(&ours), "a folder we just made shouldn't be administrator-only");
        assert!(!admin_only(&ours.join("missing")), "a file that isn't there isn't administrator-only");
        let _ = std::fs::remove_dir_all(&ours);
    }

    /// `dacl_admin_only` on a DACL made from SDDL (in memory: no file is changed).
    fn dacl_from(sddl: &str) -> bool {
        use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
        use windows_sys::Win32::Security::{ACL, GetSecurityDescriptorDacl, PSECURITY_DESCRIPTOR};
        let wide: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
        unsafe {
            let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
            let made = ConvertStringSecurityDescriptorToSecurityDescriptorW(wide.as_ptr(), SDDL_REVISION_1, &mut descriptor, null_mut());
            assert!(made != 0, "{sddl} isn't SDDL Windows takes");
            let (mut present, mut defaulted, mut dacl) = (0, 0, null_mut::<ACL>());
            assert!(GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) != 0);
            let ok = dacl_admin_only(dacl);
            LocalFree(descriptor as HLOCAL);
            ok
        }
    }

    /// Write granted to anyone but SYSTEM, Administrators or TrustedInstaller, by any kind of allow ACE that
    /// applies to the object, isn't administrator-only; read and execute, a deny, or an inherit-only ACE
    /// are. A conditional ACE, an object ACE, or a kind it doesn't know fails closed (§4.2).
    #[test]
    fn every_kind_of_allow_ace_counts() {
        let admins = "(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
        assert!(dacl_from(&format!("D:P{admins}(A;OICI;0x1200a9;;;BU)")), "Users with read and execute");
        assert!(dacl_from(&format!("D:P{admins}(D;;FA;;;WD)")), "a deny");
        assert!(dacl_from(&format!("D:P{admins}(A;OICIIO;FA;;;CO)")), "inherit-only, for CREATOR OWNER");
        assert!(!dacl_from(&format!("D:P{admins}(A;;FW;;;BU)")), "Users with write");
        assert!(!dacl_from(&format!("D:P{admins}(XA;;FA;;;AU;(Member_of {{SID(AU)}}))")), "a conditional ACE for Authenticated Users");
        assert!(!dacl_from(&format!("D:P{admins}(OA;;FA;;;AU)")), "an object ACE for Authenticated Users");
        assert!(dacl_from(&format!("D:P{admins}(XA;;FA;;;BA;(Member_of {{SID(BA)}}))")), "a conditional ACE for Administrators");
        assert!(dacl_from(&format!("D:P{admins}(XA;;0x1200a9;;;AU;(Member_of {{SID(AU)}}))")), "a conditional read");
        // no DACL at all grants everyone everything
        assert!(!dacl_from("O:BA"), "a NULL DACL");
    }

    /// Windows' own sentence for an error, without its period; "error N" for a code it has none for.
    #[test]
    fn windows_says_why() {
        let text = system_message(2);
        assert!(!text.is_empty() && !text.ends_with('.') && !text.ends_with(char::is_whitespace) && text != "error 2", "{text:?}");
        assert_eq!(system_message(0x2000_FFFF), "error 536936447");
    }

    /// The calls that start a process are only ever built outside tests (§4.3, D acceptance): no test can
    /// reach `ShellExecuteExW` or `CreateProcessW`. Each use sits under `#[cfg(not(test))]` (on `mod real`).
    #[test]
    fn launch_only_outside_tests() {
        // built up so this test's own source doesn't hold the whole name
        let needles = [["Shell", "ExecuteExW"].concat(), ["CreateProcess", "W"].concat()];
        let mut found = Vec::new();
        for (name, text) in crate::pawnio::driver::rust_files() {
            let lines: Vec<&str> = text.lines().collect();
            for (at, line) in lines.iter().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                for needle in &needles {
                    if line.contains(needle.as_str()) {
                        let cfg = lines[..at].iter().rev().find(|l| l.trim_start().starts_with("#[cfg("));
                        found.push((name.clone(), needle.clone(), cfg.map(|l| l.trim().to_string())));
                    }
                }
            }
        }
        assert!(found.len() >= 2, "the real launch calls should be in the source: {found:?}");
        for (name, needle, cfg) in &found {
            assert_eq!(cfg.as_deref(), Some("#[cfg(not(test))]"), "{needle} in {name} isn't gated to non-test");
        }
    }

    /// The checks the probe shares: tests run unelevated, and a service that isn't there says so.
    #[test]
    fn shared_checks() {
        assert!(!is_elevated(), "tests run without administrator rights (spec §6.1)");
        assert!(matches!(elevation_type(), Elevation::Default | Elevation::Limited | Elevation::Full));
        assert_eq!(service_state("WvNoSuchServiceHere"), "not installed");
        // this PC has Program Files in the registry, and a user SID
        assert!(program_files_dir().is_some_and(|p| p.to_string_lossy().contains("Program Files")));
        assert!(user_sid_string().is_some_and(|s| s.starts_with("S-1-5-21")));
        assert_eq!(pipe_name(1234), r"\\.\pipe\WonderVision.sensors.1234");
    }
}
