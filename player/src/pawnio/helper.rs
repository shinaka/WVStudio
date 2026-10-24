//! `wvsensors.exe`: the small program that runs as administrator and reads the sensors for the player, on
//! a pipe named from the player's id. It reads no data.json, playlist or media, spawns nothing, loads only
//! its two pinned module blobs, and talks only to its parent.
//!
//! The pipe carries only `{"op":"read"}` and `{"op":"quit"}` (enum `Request`): never a port, MSR, SMN or
//! module-function to pass through to the driver. The loaded modules do publish write functions, and the
//! driver checks the caller only when the device is opened, so letting the player name a function would be
//! the whole hole this design closes. A test checks the loop refuses everything else.

use std::io::{BufRead, Read, Write};

use serde_json::{Value as Json, json};

use super::Readings;

/// The two requests the pipe carries, and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    Read,
    Quit,
}

/// The most of one request line that's read (§4.4 step 10); a longer one is refused.
const MOST_LINE: u64 = 4 << 10;

/// What the reader reads the sensors with, so the request loop can be tested on a fake.
pub trait ReadSensors {
    fn read(&mut self) -> Readings;
}

impl ReadSensors for super::reader::Reader {
    fn read(&mut self) -> Readings {
        super::reader::Reader::read(self)
    }
}

/// Parses one request line. Only an object whose single field is `op` = "read" or "quit" is a request;
/// anything else (extra fields, another op, not an object) is None, so the loop answers "unknown request".
pub(crate) fn parse_request(line: &str) -> Option<Request> {
    let value: Json = serde_json::from_str(line).ok()?;
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    match object.get("op")?.as_str()? {
        "read" => Some(Request::Read),
        "quit" => Some(Request::Quit),
        _ => None,
    }
}

/// Serves the pipe: the hello, then a line of readings for each `{"op":"read"}`, until `{"op":"quit"}` or
/// EOF. Anything else gets `{"error":"unknown request"}`, with no read.
pub fn serve<R: BufRead, W: Write>(mut reader: R, mut writer: W, source: &mut dyn ReadSensors) -> std::io::Result<()> {
    writeln!(writer, "{}", json!({ "wvsensors": 1, "pid": std::process::id() }))?;
    writer.flush()?;
    loop {
        let mut line = String::new();
        let read = (&mut reader).take(MOST_LINE).read_line(&mut line)?;
        if read == 0 {
            return Ok(()); // EOF: the player let go of the pipe
        }
        match parse_request(line.trim()) {
            Some(Request::Read) => {
                let readings = serde_json::to_string(&source.read()).unwrap_or_else(|_| "{}".to_string());
                writeln!(writer, "{readings}")?;
            }
            Some(Request::Quit) => return Ok(()),
            None => writeln!(writer, "{}", json!({ "error": "unknown request" }))?,
        }
        writer.flush()?;
    }
}

/// Makes the reader's pipe, so only this user, SYSTEM and Administrators can reach it, at Medium so the
/// Medium player can write (§4.4 step 7). One first instance only: a second create of the same name fails.
pub fn create_pipe(name: &str) -> Result<std::fs::File, u32> {
    use std::os::windows::io::FromRawHandle;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    let user = super::elevation::user_sid_string().unwrap_or_else(|| "BA".to_string());
    let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;{user})S:(ML;;NW;;;ME)");
    let wname: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let wsddl: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
    unsafe {
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(wsddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, null_mut()) == 0 {
            return Err(GetLastError());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let handle = CreateNamedPipeW(
            wname.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            64 * 1024,
            4 * 1024,
            0,
            &attributes,
        );
        let error = GetLastError();
        windows_sys::Win32::Foundation::LocalFree(descriptor as windows_sys::Win32::Foundation::HLOCAL);
        if handle == INVALID_HANDLE_VALUE { Err(error) } else { Ok(std::fs::File::from_raw_handle(handle as _)) }
    }
}

/// Waits for the player to connect to `pipe` and checks it's really the parent. `Err(6)` nobody connected,
/// `Err(7)` it wasn't the parent.
pub fn accept(pipe: &std::fs::File, parent_pid: u32) -> Result<(), i32> {
    use std::os::windows::io::AsRawHandle;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError};
    use windows_sys::Win32::System::Pipes::ConnectNamedPipe;

    let handle = pipe.as_raw_handle() as _;
    let connected = unsafe { ConnectNamedPipe(handle, null_mut()) } != 0 || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
    if !connected {
        return Err(6);
    }
    if client_pid(pipe) != parent_pid {
        return Err(7);
    }
    Ok(())
}

/// The process id of whoever connected to `pipe`.
pub fn client_pid(pipe: &std::fs::File) -> u32 {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;

    let mut pid = 0u32;
    unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle() as _, &mut pid) };
    pid
}

/// The reader built on first use, so a player that never reads (the state is `asking` or declined) opens no
/// modules. `exe` is beside the module folder it loads (the installed one after an install).
#[cfg(not(test))]
struct LazyReader {
    exe: std::path::PathBuf,
    hw: std::sync::Arc<super::reader::HwLock>,
    reader: Option<super::reader::Reader>,
}

#[cfg(not(test))]
impl ReadSensors for LazyReader {
    fn read(&mut self) -> Readings {
        let hw = self.hw.clone();
        let exe = self.exe.clone();
        self.reader.get_or_insert_with(|| super::reader::Reader::this_pc(&exe, hw)).read()
    }
}

/// `wvsensors.exe <args>`: the whole reader, outside tests only.
#[cfg(not(test))]
pub fn run(args: &[String]) -> std::process::ExitCode {
    use std::io::BufReader;
    use std::process::exit;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::reader::HwLock;

    // 2: a missing or bad --parent
    let Some((install, parent_pid)) = parse_args(args) else { exit(2) };

    // 3: the parent isn't a player
    let Some(parent) = open_player_parent(parent_pid) else { exit(3) };

    // read until the parent dies, then finish the sequence in hand and stop, holding the lock so none
    // starts under the exit
    let hw = Arc::new(HwLock::default());
    watch_parent(parent, hw.clone());

    // 4: not actually elevated (a standard user's "runas" would run us as someone else)
    if !super::elevation::is_elevated() {
        exit(4);
    }
    let exe = std::env::current_exe().unwrap_or_default();
    if let Some(folder) = exe.parent() {
        let _ = std::env::set_current_dir(folder);
    }

    // 8/9/10 on a failed install; afterwards the reader loads the blobs it just installed
    let modules_exe = if install {
        match do_install(&exe) {
            Ok(installed) => installed,
            Err(code) => exit(code),
        }
    } else {
        exe.clone()
    };

    // the pipe named from the parent's id; 5: the name is taken
    let name = super::elevation::pipe_name(parent_pid);
    let Ok(pipe) = create_pipe(&name) else { exit(5) };

    // 6: nobody connects in 30 s
    let connected = Arc::new(AtomicBool::new(false));
    {
        let connected = connected.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            if !connected.load(Ordering::SeqCst) {
                exit(6);
            }
        });
    }
    // 6/7: nobody connected, or it wasn't the parent
    if let Err(code) = accept(&pipe, parent_pid) {
        exit(code);
    }
    connected.store(true, Ordering::SeqCst);

    let reader = match pipe.try_clone() {
        Ok(clone) => BufReader::new(clone),
        Err(_) => exit(5),
    };
    let mut source = LazyReader { exe: modules_exe, hw: hw.clone(), reader: None };
    let _ = serve(reader, pipe, &mut source);
    // quit or the player let go of the pipe: stop, then exit holding the lock, so no sequence is cut off
    // and none starts after (as on the parent's death)
    let _guard = hw.stop();
    exit(0)
}

/// Parses `[--install] --parent <pid>`: None for anything else.
#[cfg(not(test))]
fn parse_args(args: &[String]) -> Option<(bool, u32)> {
    let mut install = false;
    let mut parent = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--install" => install = true,
            "--parent" => parent = Some(it.next()?.parse::<u32>().ok()?),
            _ => return None,
        }
    }
    parent.map(|p| (install, p))
}

/// `--install`: copies this reader and its modules into Program Files, and gives the installed reader's
/// path, so what follows loads the blobs from there. The exit codes are install.rs's.
#[cfg(not(test))]
fn do_install(exe: &std::path::Path) -> Result<std::path::PathBuf, i32> {
    let modules = super::blobs::find_dir(exe).ok_or(super::install::DAMAGED)?;
    let program_files = super::elevation::program_files_dir().ok_or(super::install::NOT_ADMIN_ONLY)?;
    let target = program_files.join("Wonder Vision");
    super::install::install(exe, &modules, &target, &super::elevation::admin_only)?;
    Ok(target.join("wvsensors.exe"))
}

/// Opens the parent and checks it's a player (`wvplay.exe` / `wvplayw.exe`); its folder isn't checked, as
/// the installed reader serves players from any build folder. The handle is for the parent watch.
#[cfg(not(test))]
fn open_player_parent(pid: u32) -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    unsafe {
        let handle = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut buffer = vec![0u16; 1024];
        let mut length = buffer.len() as u32;
        if QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut length) == 0 {
            CloseHandle(handle);
            return None;
        }
        let path = String::from_utf16_lossy(&buffer[..length as usize]);
        let name = path.rsplit(['\\', '/']).next().unwrap_or_default().to_ascii_lowercase();
        if name == "wvplay.exe" || name == "wvplayw.exe" {
            Some(handle)
        } else {
            CloseHandle(handle);
            None
        }
    }
}

/// Watches the parent: when it dies, finish the hardware sequence in hand and stop, keeping the lock held
/// through the exit, so none is cut off and none starts after (§4.4 step 4).
#[cfg(not(test))]
fn watch_parent(parent: windows_sys::Win32::Foundation::HANDLE, hw: std::sync::Arc<super::reader::HwLock>) {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

    struct Handle(HANDLE);
    unsafe impl Send for Handle {}
    let parent = Handle(parent);
    std::thread::spawn(move || {
        let parent = parent;
        unsafe { WaitForSingleObject(parent.0, INFINITE) };
        let _guard = hw.stop();
        std::process::exit(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use serde_json::{Value as Json, json};

    /// A fake sensor source that counts how often it's read.
    #[derive(Default)]
    struct Counter(Arc<AtomicU32>);
    impl ReadSensors for Counter {
        fn read(&mut self) -> Readings {
            self.0.fetch_add(1, Ordering::SeqCst);
            Readings { cpu_temp: Some(70.375), notes: vec!["ok".into()], ..Readings::default() }
        }
    }

    /// Only an object with the single field `op` = read or quit is a request; everything else is None, so
    /// no read happens for it (S-MEDIUM-6).
    #[test]
    fn only_read_and_quit_parse() {
        assert_eq!(parse_request(r#"{"op":"read"}"#), Some(Request::Read));
        assert_eq!(parse_request(r#"{"op":"quit"}"#), Some(Request::Quit));
        let hostile = [
            r#"{"op":"write_msr"}"#,
            r#"{"op":"read","fn":"ioctl_pio_outb"}"#,
            r#"{"op":"read","port":46}"#,
            r#"{"op":"read","smn":"0x59800"}"#,
            r#"{"op":"read_smn"}"#,
            r#"{"op":"execute","function":"ioctl_read_smn"}"#,
            r#"{"op":"quit","now":true}"#,
            r#"{"op":42}"#,
            r#"{"op":["read"]}"#,
            r#"{"op":null}"#,
            r#"{"OP":"read"}"#,
            r#"{"op":"READ"}"#,
            r#"{"op":"read "}"#,
            r#"{}"#,
            r#"{"read":true}"#,
            r#"["read"]"#,
            r#""read""#,
            "read",
            "42",
            "",
            "{ not json",
        ];
        for shape in hostile {
            assert_eq!(parse_request(shape), None, "{shape}");
        }
    }

    /// A round trip over a real named pipe between two threads: hello, a read, then quit; and the pids each
    /// side sees are this process's. Nothing but reads reaches the source.
    #[test]
    fn a_pipe_round_trip() {
        let name = format!(r"\\.\pipe\wvtest.sensors.{}.{}", std::process::id(), line!());
        let pipe = create_pipe(&name).expect("create the pipe");
        let own = std::process::id();
        let count = Arc::new(AtomicU32::new(0));
        let server_count = count.clone();
        let server = std::thread::spawn(move || {
            accept(&pipe, own).expect("accept the parent");
            // the client we accepted is this process
            assert_eq!(client_pid(&pipe), own);
            let reader = BufReader::new(pipe.try_clone().unwrap());
            let mut source = Counter(server_count);
            serve(reader, pipe, &mut source).unwrap();
        });

        // the client: open the pipe, read the hello, a read round trip, hostile shapes, then quit
        let client = open_client(&name);
        assert_eq!(server_pid(&client), own);
        let mut lines = BufReader::new(client.try_clone().unwrap());
        let hello = read_line(&mut lines);
        let hello: Json = serde_json::from_str(&hello).unwrap();
        assert_eq!((hello["wvsensors"].clone(), hello["pid"].clone()), (json!(1), json!(own)));
        // a read
        let mut client = client;
        writeln!(client, r#"{{"op":"read"}}"#).unwrap();
        client.flush().unwrap();
        let answer: Json = serde_json::from_str(&read_line(&mut lines)).unwrap();
        assert_eq!(answer["cpu_temp"], json!(70.375));
        // about twenty hostile shapes, each answered "unknown request", none read
        let hostile = [
            r#"{"op":"write_msr"}"#, r#"{"op":"read","fn":"ioctl_pio_outb"}"#, r#"{"op":"read","port":46}"#,
            r#"{"op":"execute","function":"ioctl_read_smn"}"#, r#"{"op":42}"#, r#"{}"#, r#"["read"]"#, "not json",
        ];
        for shape in hostile {
            writeln!(client, "{shape}").unwrap();
            client.flush().unwrap();
            let reply: Json = serde_json::from_str(&read_line(&mut lines)).unwrap();
            assert_eq!(reply, json!({ "error": "unknown request" }), "{shape}");
        }
        // a line over 4 KiB is read capped and refused the same way (and doesn't read the sensors)
        writeln!(client, "{}", "x".repeat(5000)).unwrap();
        client.flush().unwrap();
        let capped: Json = serde_json::from_str(&read_line(&mut lines)).unwrap();
        assert_eq!(capped, json!({ "error": "unknown request" }));
        writeln!(client, r#"{{"op":"quit"}}"#).unwrap();
        client.flush().unwrap();
        server.join().unwrap();
        // only the one real read ever reached the source
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    /// A pipe is a single first instance: a second create of the same name fails.
    #[test]
    fn one_instance_only() {
        let name = format!(r"\\.\pipe\wvtest.sensors.{}.{}", std::process::id(), line!());
        let _first = create_pipe(&name).expect("the first create");
        assert!(create_pipe(&name).is_err(), "a second create of the same name should fail");
    }

    fn read_line(reader: &mut impl BufRead) -> String {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line
    }

    /// Opens the pipe as a client would, with a short wait.
    fn open_client(name: &str) -> std::fs::File {
        use std::ptr::{null, null_mut};
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT};
        use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        for _ in 0..100 {
            unsafe { WaitNamedPipeW(wide.as_ptr(), 200) };
            let handle = unsafe {
                CreateFileW(wide.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null(), OPEN_EXISTING,
                            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION, null_mut())
            };
            if handle != INVALID_HANDLE_VALUE {
                return unsafe { std::fs::File::from_raw_handle(handle as _) };
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("couldn't open {name}");
    }

    fn server_pid(file: &std::fs::File) -> u32 {
        use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
        let mut pid = 0u32;
        unsafe { GetNamedPipeServerProcessId(file.as_raw_handle() as _, &mut pid) };
        pid
    }
}
