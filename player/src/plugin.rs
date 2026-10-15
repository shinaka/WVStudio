//! Running plugins (docs/plugins.md, docs/sources.md): a program in its own folder under `plugins/`,
//! given its settings as JSON on stdin, that prints `{"values": {...}}` or `{"error": "..."}` once (poll
//! mode) or one per line as news comes (stream mode). It may only set keys under its own id. Each run is
//! in a Windows job with everything it starts, so stopping it, or the player going, stops them all.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::sleep;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{Value as Json, json};

use crate::catalog::{Manifest, Run};
use crate::data::{Source, Value};
use crate::player::log;
use crate::sources::Polled;

/// A poll run is killed after its interval, or this long at most.
const LONGEST_POLL: Duration = Duration::from_secs(30);
/// A streaming plugin that exits is restarted after this, doubling each time up to the longest wait.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const LONGEST_BACKOFF: Duration = Duration::from_secs(60);
/// The most a poll run may print to stdout in all, and the longest line a stream may print: enough for
/// any sensible set of values.
const MOST_OUTPUT: usize = 1 << 20;
const MOST_OUTPUT_TEXT: &str = "1 MiB";
/// How many of a stream's lines may wait to be taken. Past that its pipe fills and the plugin waits, so
/// one that prints faster than its lines are used can't fill the player's memory.
const WAITING_LINES: usize = 16;
/// A note shows this many characters of a key, at most.
const KEY_SHOWN: usize = 80;
/// How much of the end of stderr is kept (it's read to its end, so a plugin never blocks on it), and how
/// many characters of that an error message shows.
const STDERR_KEPT: usize = 64 << 10;
const STDERR_TAIL: usize = 300;

/// What the plugin reads on stdin: its settings, from data.json, with the manifest's defaults for
/// anything not set (a choice's first choice when it has no default).
pub fn settings(source: &Source, manifest: &Manifest) -> Json {
    let mut settings = serde_json::Map::new();
    for s in &manifest.settings {
        let value = source.setting(&s.key).or_else(|| s.fallback()).unwrap_or(Json::Null);
        settings.insert(s.key.clone(), value);
    }
    json!({ "settings": settings })
}

/// Runs the plugin once (the Hub's poll, and /test). A streaming plugin is run until its first line.
pub fn poll(source: &Source, manifest: &Manifest) -> Result<Polled, String> {
    let run = manifest.run.as_ref().ok_or("not a plugin")?;
    let timeout = source.every().min(LONGEST_POLL);
    let mut plugin = spawn(run, &settings(source, manifest).to_string())?;
    let (lines, errors) = readers(&mut plugin.child, !manifest.streams());
    if manifest.streams() {
        // the plugin is stopped when it's dropped, at the end
        return match lines.recv_timeout(timeout) {
            Ok(Out::Line(line)) => parse(&manifest.id, &line),
            Ok(Out::TooMuch) => Err(format!("printed a line longer than {MOST_OUTPUT_TEXT}")),
            Err(RecvTimeoutError::Timeout) => Err(format!("printed nothing in {}", seconds(timeout))),
            Err(RecvTimeoutError::Disconnected) => Err(exited(&mut plugin, &errors)),
        };
    }
    let Some(status) = wait(&mut plugin.child, timeout) else {
        return Err(format!("took longer than {}", seconds(timeout)));
    };
    plugin.stop(); // anything it left running, so its output ends
    let mut output = Vec::new();
    for out in lines.iter() {
        match out {
            Out::Line(line) => output.push(line),
            Out::TooMuch => return Err(format!("printed more than {MOST_OUTPUT_TEXT}")),
        }
    }
    let output = output.join("\n");
    let stderr = errors.recv().unwrap_or_default();
    match last_json(&output) {
        Some(j) => parse_json(&manifest.id, &j),
        None if !status.success() => Err(format!("exited with code {}{}", status.code().unwrap_or(-1), tail(&stderr))),
        None => Err(format!("didn't print JSON{}", tail(if output.trim().is_empty() { &stderr } else { &output }))),
    }
}

/// Keeps a streaming plugin running until `stop`, reporting each line it prints, and restarting it
/// with a back-off when it exits. `stop` is checked on every line, and after each quarter second of
/// silence.
pub fn stream(source: &Source, manifest: &Manifest, stop: &AtomicBool, mut report: impl FnMut(Result<Polled, String>)) {
    let Some(run) = manifest.run.as_ref() else { return report(Err("not a plugin".into())) };
    let input = settings(source, manifest).to_string();
    let stopped = || stop.load(Ordering::Relaxed);
    let mut backoff = FIRST_BACKOFF;
    while !stopped() {
        match spawn(run, &input) {
            Err(e) => report(Err(e)),
            Ok(mut plugin) => {
                let (lines, errors) = readers(&mut plugin.child, false);
                loop {
                    match lines.recv_timeout(Duration::from_millis(250)) {
                        _ if stopped() => return, // dropping the plugin stops it, and all it started
                        Ok(out) => {
                            let result = match out {
                                Out::Line(line) => parse(&manifest.id, &line),
                                Out::TooMuch => Err(format!("printed a line longer than {MOST_OUTPUT_TEXT}")),
                            };
                            if result.is_ok() {
                                backoff = FIRST_BACKOFF; // it's working: a later crash gets a quick restart
                            }
                            report(result);
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break, // its stdout closed: it's going or gone
                    }
                }
                report(Err(format!("{}; restarting in {}", exited(&mut plugin, &errors), seconds(backoff))));
            }
        }
        if !pause(backoff, stop) {
            return;
        }
        backoff = (backoff * 2).min(LONGEST_BACKOFF);
    }
}

/// A plugin's program, running in a job with everything it starts.
struct Plugin {
    child: Child,
    /// None only when Windows wouldn't make or assign the job: then only the program itself is stopped.
    job: Option<win::Job>,
}

impl Plugin {
    /// Stops the program and everything it started, and waits for the program to go.
    fn stop(&mut self) {
        match &self.job {
            Some(job) => job.terminate(),
            None => {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts the command in the plugin's folder, with no window, in a job of its own, and feeds it `input`
/// on stdin.
fn spawn(run: &Run, input: &str) -> Result<Plugin, String> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
    let mut command = Command::new(&run.command[0]);
    command.args(&run.command[1..]).current_dir(&run.dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // suspended until it's in its job, so nothing it starts can be left out of the job
    command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let child = command.spawn().map_err(|e| format!("couldn't run {:?}: {e}", run.command[0]))?;
    let job = win::Job::new().and_then(|job| job.assign(&child).map(|()| job));
    if let Err(e) = &job {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            log(format_args!("plugins: no job for {:?} ({e}): stopping a plugin stops only its own program", run.command[0]));
        }
    }
    let mut plugin = Plugin { child, job: job.ok() };
    if !win::resume(plugin.child.id()) {
        return Err(format!("couldn't start {:?}", run.command[0])); // dropped, so killed
    }
    if let Some(mut stdin) = plugin.child.stdin.take() {
        // on its own thread: the settings may not fit the pipe before the plugin reads them
        let input = input.to_string();
        std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
            let _ = stdin.write_all(b"\n");
        });
    }
    Ok(plugin)
}

/// What the stdout reader passes on.
enum Out {
    /// A line, as UTF-8, with any byte that isn't replaced by U+FFFD.
    Line(String),
    /// More than MOST_OUTPUT: one line of a stream (skipped), or a poll run's output in all.
    TooMuch,
}

/// Reads the child's stdout line by line and the end of its stderr, each on a thread, so neither pipe
/// fills up while the other is waited on. Both are read to their end, so a plugin never fails on a
/// closed pipe. `per_run`: a poll run's output, whose whole is capped, rather than a stream's, whose
/// lines each are.
///
/// A poll run's lines are only taken once it has exited, so all of them wait (1 MiB at most). A
/// stream's are taken as they come, and only WAITING_LINES may wait: past that the reader waits too,
/// and the full pipe holds the plugin back until the player catches up.
fn readers(child: &mut Child, per_run: bool) -> (Receiver<Out>, Receiver<String>) {
    type PassOn = Box<dyn Fn(Out) -> bool + Send>;
    let (send, out): (PassOn, _) = if per_run {
        let (lines, out) = mpsc::channel();
        (Box::new(move |o| lines.send(o).is_ok()), out)
    } else {
        let (lines, out) = mpsc::sync_channel(WAITING_LINES);
        (Box::new(move |o| lines.send(o).is_ok()), out)
    };
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || read_lines(stdout, per_run, send));
    }
    let (errors, err) = mpsc::channel();
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let _ = errors.send(read_tail(stderr));
        });
    }
    (out, err)
}

/// Passes each line on through `send`, which is false once nothing takes them any more.
fn read_lines(stdout: impl Read, per_run: bool, send: impl Fn(Out) -> bool) {
    let mut reader = BufReader::new(stdout);
    let (mut line, mut too_long, mut total) = (Vec::new(), false, 0usize);
    loop {
        let (used, ended) = match reader.fill_buf() {
            Ok([]) => break,
            Ok(buf) => {
                let end = buf.iter().position(|&b| b == b'\n');
                let part = &buf[..end.unwrap_or(buf.len())];
                if !too_long && line.len() + part.len() > MOST_OUTPUT {
                    too_long = true;
                    line = Vec::new();
                } else if !too_long {
                    line.extend_from_slice(part);
                }
                (end.map_or(buf.len(), |e| e + 1), end.is_some())
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        reader.consume(used);
        total += used;
        if per_run && total > MOST_OUTPUT {
            send(Out::TooMuch);
            break;
        }
        if ended {
            let out = if too_long { Out::TooMuch } else { Out::Line(decode(&line)) };
            if !send(out) {
                break;
            }
            (line, too_long) = (Vec::new(), false);
        }
    }
    if total <= MOST_OUTPUT || !per_run {
        // the last line, if it had no line break
        if too_long {
            send(Out::TooMuch);
        } else if !line.is_empty() {
            send(Out::Line(decode(&line)));
        }
    }
    // whatever's left drains to nowhere
    let _ = std::io::copy(&mut reader, &mut std::io::sink());
}

/// A line of output as text: one bad byte costs one character, not the line.
fn decode(line: &[u8]) -> String {
    String::from_utf8_lossy(line).trim_end_matches('\r').to_string()
}

/// The end of stderr (its last STDERR_KEPT bytes), read to its end, as UTF-8 with any byte that isn't
/// replaced by U+FFFD.
fn read_tail(mut stderr: impl Read) -> String {
    let (mut kept, mut chunk) = (Vec::new(), [0u8; 8192]);
    loop {
        match stderr.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                kept.extend_from_slice(&chunk[..n]);
                if kept.len() > 2 * STDERR_KEPT {
                    kept.drain(..kept.len() - STDERR_KEPT);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&kept[kept.len().saturating_sub(STDERR_KEPT)..]).into_owned()
}

/// Once the program's stdout has closed: gives it a moment to exit, stops whatever is left of it, and
/// says how it ended, with the end of its stderr.
fn exited(plugin: &mut Plugin, errors: &Receiver<String>) -> String {
    let status = wait(&mut plugin.child, Duration::from_secs(1));
    plugin.stop();
    let code = status.or_else(|| plugin.child.wait().ok()).and_then(|s| s.code()).unwrap_or(-1);
    let stderr = errors.recv().unwrap_or_default();
    format!("exited with code {code}{}", tail(&stderr))
}

/// Waits for the child to exit, or gives up after `timeout`.
fn wait(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => sleep(Duration::from_millis(20)),
            _ => return None,
        }
    }
}

/// The JSON object a poll run printed, pretty-printed or not. Stray prints before it are forgiven: the
/// last object that starts a line is the one.
fn last_json(output: &str) -> Option<Json> {
    // the lines whose first character that isn't a space is a `{`, found in one pass
    let output = output.trim_end();
    let (mut starts, mut at) = (Vec::new(), 0);
    for line in output.split('\n') {
        let indent = line.len() - line.trim_start().len();
        if line[indent..].starts_with('{') {
            starts.push(at + indent);
        }
        at += line.len() + 1;
    }
    starts.into_iter().rev().find_map(|i| serde_json::from_str(&output[i..]).ok())
}

/// Sleeps for `how_long` unless stopped first; false once stopped.
fn pause(how_long: Duration, stop: &AtomicBool) -> bool {
    let until = Instant::now() + how_long;
    while Instant::now() < until {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        sleep(Duration::from_millis(100));
    }
    !stop.load(Ordering::Relaxed)
}

fn seconds(d: Duration) -> String {
    format!("{} s", d.as_secs_f64().round())
}

/// The last STDERR_TAIL characters of what a plugin wrote, for an error message, on one line.
fn tail(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    // where the last STDERR_TAIL characters start: always a character boundary
    let start = text.char_indices().rev().nth(STDERR_TAIL - 1).map_or(0, |(i, _)| i);
    let shown: String = text[start..].lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" | ");
    format!(": {}{shown}", if start > 0 { "..." } else { "" })
}

/// One line of plugin output.
fn parse(id: &str, line: &str) -> Result<Polled, String> {
    let j: Json = serde_json::from_str(line).map_err(|_| format!("didn't print JSON{}", tail(line)))?;
    parse_json(id, &j)
}

/// `{"values": {...}}` becomes the values under `<id>.` that are numbers, text or ISO times; anything
/// else is noted and left out. `{"error": "..."}` is the plugin saying it failed; `"error": null` is no
/// error.
fn parse_json(id: &str, j: &Json) -> Result<Polled, String> {
    if let Some(e) = j.get("error").filter(|e| !e.is_null()) {
        return Err(e.as_str().map_or_else(|| e.to_string(), |s| s.to_string()));
    }
    let given = j.get("values").and_then(Json::as_object).ok_or("the output has neither \"values\" nor \"error\"")?;
    let prefix = format!("{id}.");
    let (mut values, mut rejected) = (Vec::new(), Vec::new());
    for (key, v) in given {
        if !key.starts_with(&prefix) || key.len() == prefix.len() {
            rejected.push(format!("{} isn't under {prefix}", shown_key(key)));
            continue;
        }
        let value = match v {
            Json::Number(n) => n.as_f64().map(Value::Number),
            Json::String(s) => Some(match DateTime::parse_from_rfc3339(s) {
                Ok(t) => Value::Time(t.with_timezone(&Utc)),
                Err(_) => Value::Text(s.clone()),
            }),
            _ => None,
        };
        match value {
            Some(value) => values.push((key.clone(), value)),
            None => rejected.push(format!("{} isn't a number, text or ISO time", shown_key(key))),
        }
    }
    let mut notes: Vec<String> = rejected.iter().take(5).map(|r| format!("ignored {r}")).collect();
    if rejected.len() > 5 {
        notes.push(format!("and {} more ignored", rejected.len() - 5));
    }
    // logged by the caller, once each time it changes, like any failure
    if values.is_empty() && !rejected.is_empty() {
        return Err(format!("nothing usable: {}", notes.join("; ")));
    }
    Ok(Polled { values, notes })
}

/// A key as a note shows it: its first KEY_SHOWN characters, then `...` if there's more.
fn shown_key(key: &str) -> String {
    match key.char_indices().nth(KEY_SHOWN) {
        Some((cut, _)) => format!("{}...", &key[..cut]),
        None => key.to_string(),
    }
}

/// The Windows calls: a job that takes a process and everything it starts, and resuming a process
/// started suspended.
mod win {
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::ptr::null;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    fn last_error() -> String {
        std::io::Error::last_os_error().to_string()
    }

    /// A job whose processes are all killed when it's terminated, or closed: by dropping it, or by the
    /// player exiting, however it exits. Its handle isn't inheritable, so no plugin holds it open.
    pub struct Job(HANDLE);

    // a job's handle can be used from any thread
    unsafe impl Send for Job {}

    impl Job {
        pub fn new() -> Result<Job, String> {
            unsafe {
                let handle = CreateJobObjectW(null(), null());
                if handle.is_null() {
                    return Err(last_error());
                }
                let job = Job(handle);
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set = SetInformationJobObject(handle, JobObjectExtendedLimitInformation, &limits as *const _ as *const c_void,
                                                  size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32);
                if set == 0 {
                    return Err(last_error());
                }
                Ok(job)
            }
        }

        /// Puts the child in the job: from then on, whatever it starts is in the job too.
        pub fn assign(&self, child: &Child) -> Result<(), String> {
            match unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) } {
                0 => Err(last_error()),
                _ => Ok(()),
            }
        }

        /// Kills every process in the job.
        pub fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Resumes a process started suspended (its one thread); false if it couldn't be.
    pub fn resume(pid: u32) -> bool {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return false;
            }
            let mut entry: THREADENTRY32 = zeroed();
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            let mut resumed = false;
            let mut more = Thread32First(snapshot, &mut entry) != 0;
            while more {
                if entry.th32OwnerProcessID == pid {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if !thread.is_null() {
                        resumed |= ResumeThread(thread) != u32::MAX;
                        CloseHandle(thread);
                    }
                }
                more = Thread32Next(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
            resumed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A folder of throwaway plugins, gone when the test is.
    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Folder {
            let dir = std::env::temp_dir().join(format!("wvplay-plugin-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Folder(dir)
        }

        /// A plugin whose program is a .cmd (batch) file, or a Python script.
        fn plugin(&self, id: &str, mode: &str, file: &str, body: &str) -> Manifest {
            let dir = self.0.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(file), body).unwrap();
            let command = match file.ends_with(".py") {
                true => vec!["python".to_string(), "-I".into(), file.into()],
                false => vec!["cmd".to_string(), "/c".into(), format!(".\\{file}")],
            };
            Manifest {
                id: id.into(),
                name: id.into(),
                description: String::new(),
                interval: Default::default(),
                settings: vec![serde_json::from_value(json!({ "key": "city", "label": "", "type": "text", "default": "Oslo" })).unwrap(),
                               serde_json::from_value(json!({ "key": "secret", "label": "", "type": "password" })).unwrap()],
                provides: Vec::new(),
                run: Some(Run { command, mode: mode.into(), dir }),
            }
        }

        /// Another file in a plugin's folder, byte for byte.
        fn file(&self, id: &str, name: &str, bytes: &[u8]) {
            std::fs::write(self.0.join(id).join(name), bytes).unwrap();
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn source(id: &str, interval: f64) -> Source {
        serde_json::from_value(json!({ "type": id, "interval": interval, "secret": "hunter2" })).unwrap()
    }

    fn python() -> bool {
        Command::new("python").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    /// Runs a stream until `done` says so, given each report so far; gives the reports.
    fn stream_until(m: &Manifest, done: impl Fn(&[Result<Polled, String>]) -> bool) -> Vec<Result<Polled, String>> {
        let stop = AtomicBool::new(false);
        let mut got = Vec::new();
        stream(&source(&m.id, 60.0), m, &stop, |r| {
            got.push(r);
            if done(&got) {
                stop.store(true, Ordering::Relaxed);
            }
        });
        got
    }

    /// Whether a process is still running, now.
    fn running(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
        unsafe {
            let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if process.is_null() {
                return false;
            }
            let running = WaitForSingleObject(process, 0) == WAIT_TIMEOUT;
            CloseHandle(process);
            running
        }
    }

    /// Whether a process has ended, or does within 5 s.
    fn ends(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while running(pid) {
            if Instant::now() > deadline {
                return false;
            }
            sleep(Duration::from_millis(50));
        }
        true
    }

    #[test]
    fn settings_with_defaults() {
        let f = Folder::new("settings");
        let mut m = f.plugin("w", "poll", "p.cmd", "");
        assert_eq!(settings(&source("w", 60.0), &m), json!({ "settings": { "city": "Oslo", "secret": "hunter2" } }));
        let given: Source = serde_json::from_value(json!({ "type": "w", "city": "Rome", "other": 1 })).unwrap();
        assert_eq!(settings(&given, &m)["settings"], json!({ "city": "Rome", "secret": null }));
        // a choice with no default gets its first choice
        m.settings.push(serde_json::from_value(json!({ "key": "units", "label": "", "type": "choice", "choices": ["c", "f"] })).unwrap());
        assert_eq!(settings(&given, &m)["settings"]["units"], json!("c"));
        // keys a built-in uses are the plugin's own: its default, and an empty value kept
        m.settings.push(serde_json::from_value(json!({ "key": "hwinfo", "label": "", "type": "bool", "default": true })).unwrap());
        m.settings.push(serde_json::from_value(json!({ "key": "url", "label": "", "type": "text", "default": "http://x" })).unwrap());
        let given: Source = serde_json::from_value(json!({ "type": "w", "url": "" })).unwrap();
        assert_eq!(settings(&given, &m)["settings"], json!({ "city": "Oslo", "secret": null, "units": "c", "hwinfo": true, "url": "" }));
    }

    #[test]
    fn poll_output() {
        let f = Folder::new("poll");
        // echoes a value and a key it may not set; reads its settings from stdin
        let m = f.plugin("w", "poll", "p.cmd", "@echo off\r\nset /p in=\r\necho {\"values\": {\"w.temp\": 21.5, \"w.at\": \"2026-10-14T10:00:00Z\", \"w.desc\": \"Clear\", \"claude.week\": 1, \"w.list\": [1]}}\r\n");
        let polled = poll(&source("w", 60.0), &m).unwrap();
        let get = |key: &str| polled.values.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(polled.values.len(), 3);
        assert_eq!(get("w.temp"), Some(Value::Number(21.5)));
        assert!(matches!(get("w.at"), Some(Value::Time(_))));
        assert_eq!(get("w.desc"), Some(Value::Text("Clear".into())));
        assert_eq!(polled.notes, ["ignored claude.week isn't under w.", "ignored w.list isn't a number, text or ISO time"]);
        // pretty-printed output, after a stray line
        let m = f.plugin("w", "poll", "p.cmd", "@echo starting\r\n@echo {\"values\":\r\n@echo  {\"w.n\": 1}\r\n@echo }\r\n");
        assert_eq!(poll(&source("w", 60.0), &m).unwrap().values, [("w.n".into(), Value::Number(1.0))]);
        // its own error, no JSON at all, and a crash
        let m = f.plugin("w", "poll", "p.cmd", "@echo {\"error\": \"no network\"}\r\n");
        assert_eq!(poll(&source("w", 60.0), &m).unwrap_err(), "no network");
        let m = f.plugin("w", "poll", "p.cmd", "@echo hello world\r\n");
        assert_eq!(poll(&source("w", 60.0), &m).unwrap_err(), "didn't print JSON: hello world");
        let m = f.plugin("w", "poll", "p.cmd", "@echo oops 1>&2\r\n@exit 3\r\n");
        assert_eq!(poll(&source("w", 60.0), &m).unwrap_err(), "exited with code 3: oops");
        // only its own keys count
        let m = f.plugin("w", "poll", "p.cmd", "@echo {\"values\": {\"pc.cpu.load\": 100, \"w\": 1}}\r\n");
        assert!(poll(&source("w", 60.0), &m).unwrap_err().starts_with("nothing usable: ignored pc.cpu.load isn't under w.;"));
        // a command that isn't there
        let mut m = f.plugin("w", "poll", "p.cmd", "");
        m.run.as_mut().unwrap().command = vec!["no-such-program-wvplay".into()];
        assert!(poll(&source("w", 60.0), &m).unwrap_err().starts_with("couldn't run"));
        // more than 1 MiB in all
        let m = f.plugin("w", "poll", "p.cmd", "@type big.txt\r\n@echo {\"values\": {\"w.n\": 1}}\r\n");
        f.file("w", "big.txt", &b"0123456789abcdef\r\n".repeat(70_000));
        assert_eq!(poll(&source("w", 60.0), &m).unwrap_err(), "printed more than 1 MiB");
    }

    #[test]
    fn poll_timeout() {
        let f = Folder::new("timeout");
        let m = f.plugin("slow", "poll", "p.cmd", "@ping -n 30 127.0.0.1 > nul\r\n@echo {\"values\": {\"slow.n\": 1}}\r\n");
        let start = Instant::now();
        // the interval is the timeout (5 s is the least interval)
        assert_eq!(poll(&source("slow", 5.0), &m).unwrap_err(), "took longer than 5 s");
        assert!(start.elapsed() < Duration::from_secs(20));
    }

    /// A plugin started through a wrapper (cmd running a .cmd that runs Python) is stopped whole: on a
    /// poll's time-out, when a stream stops (a restart), and when its job is closed (the player exiting).
    #[test]
    fn wrappers_are_stopped_whole() {
        if !python() {
            eprintln!("no python: skipping");
            return;
        }
        let f = Folder::new("wrapper");
        // a poll run whose Python outlives the time-out, and says who it is in a file
        let m = f.plugin("t", "poll", "t.cmd",
            "@python -c \"import os, time; f = open('pid.txt', 'w'); f.write(str(os.getpid())); f.close(); time.sleep(60)\"\r\n");
        assert_eq!(poll(&source("t", 5.0), &m).unwrap_err(), "took longer than 5 s");
        let pid: u32 = std::fs::read_to_string(f.0.join("t").join("pid.txt")).unwrap().parse().unwrap();
        assert!(ends(pid), "the poll's Python {pid} is still running");

        // a stream whose Python prints who it is, then runs on
        let m = f.plugin("s", "stream", "s.cmd",
            "@python -c \"import json, os, time; print(json.dumps({'values': {'s.pid': os.getpid()}}), flush=True); time.sleep(60)\"\r\n");
        let got = stream_until(&m, |got| got.iter().any(Result::is_ok));
        let pid = match &got[..] {
            [Ok(p)] => match p.values[0].1 { Value::Number(n) => n as u32, _ => panic!("{:?}", p.values) },
            other => panic!("{other:?}"),
        };
        assert!(ends(pid), "the stream's Python {pid} is still running");

        // the job closing, as when the player exits, stops it all too
        let mut plugin = spawn(m.run.as_ref().unwrap(), "{}").unwrap();
        let (lines, _errors) = readers(&mut plugin.child, false);
        let Ok(Out::Line(line)) = lines.recv_timeout(Duration::from_secs(20)) else { panic!("no line") };
        let pid = parse("s", &line).unwrap().values[0].1.clone();
        let Value::Number(pid) = pid else { panic!("{pid:?}") };
        assert!(running(pid as u32));
        drop(plugin.job.take());
        assert!(ends(pid as u32), "Python {pid} outlived its job");
    }

    #[test]
    fn streaming() {
        if !python() {
            eprintln!("no python: skipping");
            return;
        }
        let f = Folder::new("stream");
        // prints its settings' city twice, then exits with 1
        let m = f.plugin("s", "stream", "s.py", "import json, sys, time\n\
            s = json.loads(sys.stdin.readline())['settings']\n\
            for i in range(2):\n    print(json.dumps({'values': {'s.city': s['city'], 's.n': i}}), flush=True)\n    time.sleep(0.2)\n\
            print('bye', file=sys.stderr)\nsys.exit(1)\n");
        let start = Instant::now();
        // two runs: 2 values, an exit, a 1 s wait, 2 values, an exit
        let got: Vec<_> = stream_until(&m, |got| got.len() == 6).into_iter().map(|r| r.map(|p| p.values)).collect();
        assert_eq!(got.len(), 6);
        assert_eq!(got[0], Ok(vec![("s.city".into(), Value::Text("Oslo".into())), ("s.n".into(), Value::Number(0.0))]));
        assert_eq!(got[1].as_ref().unwrap()[1].1, Value::Number(1.0));
        assert_eq!(got[2], Err("exited with code 1: bye; restarting in 1 s".into()));
        assert!(got[3].is_ok() && got[4].is_ok() && got[5].is_err());
        assert!(start.elapsed() >= Duration::from_secs(1) && start.elapsed() < Duration::from_secs(10), "{:?}", start.elapsed());
        // /test on a streaming plugin: its first line
        let once = poll(&source("s", 60.0), &m).unwrap();
        assert_eq!(once.values[1], ("s.n".into(), Value::Number(0.0)));
        // nothing printed in time
        let quiet = f.plugin("q", "stream", "q.py", "import time\ntime.sleep(30)\n");
        assert_eq!(poll(&source("q", 5.0), &quiet).unwrap_err(), "printed nothing in 5 s");
    }

    /// A stream that exits before its first line says so straight away on /test.
    #[test]
    fn stream_test_that_exits() {
        let f = Folder::new("exits");
        let m = f.plugin("x", "stream", "s.cmd", "@echo broken 1>&2\r\n@exit 4\r\n");
        let start = Instant::now();
        assert_eq!(poll(&source("x", 60.0), &m).unwrap_err(), "exited with code 4: broken");
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    /// Output that isn't UTF-8 (Python's print on Windows writes the ANSI code page) costs only its
    /// bad characters: the JSON after it, and the end of stderr, still come through.
    #[test]
    fn output_that_isnt_utf8() {
        let f = Folder::new("lossy");
        // "café starting" and "Zürich" in Windows-1252
        let m = f.plugin("l", "poll", "p.cmd", "@type out.bin\r\n@type err.bin 1>&2\r\n");
        f.file("l", "out.bin", b"caf\xe9 starting\r\n{\"values\": {\"l.city\": \"Z\xfcrich\"}}\r\n");
        f.file("l", "err.bin", b"warning: Z\xfcrich\r\n");
        assert_eq!(poll(&source("l", 60.0), &m).unwrap().values, [("l.city".into(), Value::Text("Z\u{FFFD}rich".into()))]);
        let m = f.plugin("l", "poll", "p.cmd", "@type err.bin 1>&2\r\n@exit 2\r\n");
        assert_eq!(poll(&source("l", 60.0), &m).unwrap_err(), "exited with code 2: warning: Z\u{FFFD}rich");
        // in a stream, the bad line is one bad line, and the next one counts
        let m = f.plugin("s", "stream", "s.cmd", "@type out.bin\r\n@ping -n 30 127.0.0.1 > nul\r\n");
        f.file("s", "out.bin", b"caf\xe9 starting\r\n{\"values\": {\"s.n\": 1}}\r\n");
        let got = stream_until(&m, |got| got.iter().any(Result::is_ok));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].as_ref().unwrap_err(), "didn't print JSON: caf\u{FFFD} starting");
        assert_eq!(got[1].as_ref().unwrap().values, [("s.n".into(), Value::Number(1.0))]);
    }

    /// A stream may print as much as it likes over its life, on stdout and stderr: only a line over
    /// 1 MiB is refused, and it isn't restarted for any of it.
    #[test]
    fn long_streams() {
        let f = Folder::new("long");
        let m = f.plugin("b", "stream", "s.cmd", "@type big.txt\r\n@type big.txt 1>&2\r\n@type end.txt\r\n@ping -n 30 127.0.0.1 > nul\r\n");
        let mut big = vec![b'x'; MOST_OUTPUT + 10];
        big.extend_from_slice(b"\r\n");
        for i in 0..12_000 {
            big.extend_from_slice(format!("{{\"values\": {{\"b.n\": {i}, \"b.pad\": \"{}\"}}}}\r\n", "x".repeat(60)).as_bytes());
        }
        assert!(big.len() > 2 * MOST_OUTPUT);
        f.file("b", "big.txt", &big);
        f.file("b", "end.txt", b"{\"values\": {\"b.end\": 1}}\r\n");
        let start = Instant::now();
        let got = stream_until(&m, |got| got.last().is_some_and(|r| r.as_ref().is_ok_and(|p| p.values[0].0 == "b.end")));
        let errors: Vec<&String> = got.iter().filter_map(|r| r.as_ref().err()).collect();
        assert_eq!(errors, ["printed a line longer than 1 MiB"]);
        assert_eq!(got.len(), 12_002);
        assert!(start.elapsed() < Duration::from_secs(30), "{:?}", start.elapsed());
    }

    /// A stream that prints faster than its lines are taken is held back: the player keeps only a few
    /// of its lines waiting, and the full pipe makes the program wait for the rest.
    #[test]
    fn streams_are_held_back() {
        let f = Folder::new("held");
        let m = f.plugin("h", "stream", "s.cmd", "@type big.txt\r\n@echo done> done.txt\r\n@ping -n 30 127.0.0.1 > nul\r\n");
        f.file("h", "big.txt", "{\"values\": {\"h.n\": 1}}\r\n".repeat(100_000).as_bytes());
        let done = f.0.join("h").join("done.txt");
        let mut plugin = spawn(m.run.as_ref().unwrap(), "{}").unwrap();
        let (lines, _errors) = readers(&mut plugin.child, false);
        sleep(Duration::from_secs(2));
        assert!(!done.exists(), "it printed 2.4 MB while nothing was taken");
        for _ in 0..100_000 {
            assert!(matches!(lines.recv_timeout(Duration::from_secs(10)), Ok(Out::Line(_))));
        }
        // once its lines are taken, it goes on
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done.exists() {
            assert!(Instant::now() < deadline, "it never finished printing");
            sleep(Duration::from_millis(50));
        }
    }

    /// A stream that never pauses is still stopped at once, and reports nothing after.
    #[test]
    fn stopping_a_busy_stream() {
        let f = Folder::new("busy");
        let m = f.plugin("f", "stream", "s.cmd", "@echo off\r\n:again\r\necho {\"values\": {\"f.n\": 1}}\r\ngoto again\r\n");
        let start = Instant::now();
        let got = stream_until(&m, |got| got.len() == 3);
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(Result::is_ok));
        assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
    }

    #[test]
    fn parsing() {
        assert_eq!(parse("w", "{\"error\": \"x\"}").unwrap_err(), "x");
        assert_eq!(parse("w", "{\"error\": {\"code\": 1}}").unwrap_err(), "{\"code\":1}");
        assert_eq!(parse("w", "{}").unwrap_err(), "the output has neither \"values\" nor \"error\"");
        assert_eq!(parse("w", "nope").unwrap_err(), "didn't print JSON: nope");
        let p = parse("w", "{\"values\": {\"w.a\": 1, \"w.b\": true, \"w.c\": null, \"w.d\": \"2026-01-02T03:04:05+01:00\"}}").unwrap();
        assert_eq!(p.values.len(), 2);
        assert_eq!(p.notes.len(), 2);
        // "error": null is no error
        let p = parse("w", "{\"values\": {\"w.a\": 1}, \"error\": null}").unwrap();
        assert_eq!(p.values, [("w.a".into(), Value::Number(1.0))]);
        assert_eq!(parse("w", "{\"error\": null}").unwrap_err(), "the output has neither \"values\" nor \"error\"");
        assert!(Path::new(".").exists());
        // a note shows the start of a long key
        let long = format!("{{\"values\": {{\"{}\": 1, \"w.{}\": [1]}}}}", "k".repeat(200), "é".repeat(200));
        assert_eq!(parse("w", &long).unwrap_err(), format!("nothing usable: ignored {}... isn't under w.; ignored w.{}... isn't a number, text or ISO time",
                                                         "k".repeat(80), "é".repeat(78)));
    }

    /// The JSON in a poll run's output is the last object that starts a line, found quickly whatever
    /// the output looks like.
    #[test]
    fn finding_the_json() {
        assert_eq!(last_json("starting\n  {\"a\": 1}\n\n"), Some(json!({ "a": 1 })));
        assert_eq!(last_json("{\"a\": 1}\r\n{\"b\":\r\n 2}\r\n  "), Some(json!({ "b": 2 })));
        assert_eq!(last_json("caf\u{e9} {\n\t{\"c\": 3}"), Some(json!({ "c": 3 })));
        // a brace that doesn't start a line isn't a start, and anything after the JSON breaks it
        assert_eq!(last_json("x {\"a\": 1}"), None);
        assert_eq!(last_json("{\"a\": 1}\nx {\"b\": 2}"), None);
        // a long line of braces, or many short ones, is given up on at once
        let start = Instant::now();
        assert_eq!(last_json(&"{".repeat(MOST_OUTPUT)), None);
        assert_eq!(last_json(&"{\n".repeat(MOST_OUTPUT / 2)), None);
        assert_eq!(last_json(&format!("{}{}", "{\n".repeat(1000), " ".repeat(MOST_OUTPUT))), None);
        assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
    }

    #[test]
    fn tails() {
        assert_eq!(tail("a\n\nb\n"), ": a | b");
        assert_eq!(tail("  "), "");
        let a300 = "a".repeat(300);
        assert_eq!(tail(&a300), format!(": {a300}"));
        // the last 300 characters, cut between characters, never inside one
        assert_eq!(tail(&format!("ü{a300}")), format!(": ...{a300}"));
        assert_eq!(tail(&format!("aü{}", "a".repeat(299))), format!(": ...ü{}", "a".repeat(299)));
        let u = "ü".repeat(400);
        assert_eq!(tail(&u), format!(": ...{}", "ü".repeat(300)));
        assert_eq!(tail(&format!("{}°C", "x".repeat(400))).chars().count(), ": ...".len() + 300);
    }
}
