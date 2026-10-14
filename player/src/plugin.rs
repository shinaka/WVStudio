//! Running plugins (docs/sources.md): a program in its own folder under `plugins/`, given its settings
//! as JSON on stdin, that prints `{"values": {...}}` or `{"error": "..."}` once (poll mode) or one per
//! line as news comes (stream mode). It may only set keys under its own id.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
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
/// How much of a plugin's output is kept: enough for any sensible set of values.
const MOST_OUTPUT: u64 = 1 << 20;
const STDERR_TAIL: usize = 300;
/// A line of a stream longer than this is bad output, not a value.
const LONGEST_LINE: usize = 1 << 20;

/// What the plugin reads on stdin: its settings, from data.json, with the manifest's defaults for
/// anything not set.
pub fn settings(source: &Source, manifest: &Manifest) -> Json {
    let mut settings = serde_json::Map::new();
    for s in &manifest.settings {
        let value = source.setting(&s.key).or_else(|| s.default.clone()).unwrap_or(Json::Null);
        settings.insert(s.key.clone(), value);
    }
    json!({ "settings": settings })
}

/// Runs the plugin once (the Hub's poll, and /test). A streaming plugin is run until its first line.
pub fn poll(source: &Source, manifest: &Manifest) -> Result<Polled, String> {
    let run = manifest.run.as_ref().ok_or("not a plugin")?;
    let timeout = source.every().min(LONGEST_POLL);
    let input = settings(source, manifest).to_string();
    let mut child = spawn(run, &input)?;
    let (lines, errors) = readers(&mut child);
    if manifest.streams() {
        let first = match lines.recv_timeout(timeout) {
            Ok(line) => parse(&manifest.id, &line),
            Err(_) => Err(format!("printed nothing in {}", seconds(timeout))),
        };
        let _ = child.kill();
        let _ = child.wait();
        return first;
    }
    let status = match wait(&mut child, timeout) {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("took longer than {}", seconds(timeout)));
        }
    };
    let output: Vec<String> = lines.iter().collect();
    let output = output.join("\n");
    let stderr = errors.recv().unwrap_or_default();
    match last_json(&output) {
        Some(j) => parse_json(&manifest.id, &j),
        None if !status.success() => Err(format!("exited with code {}{}", status.code().unwrap_or(-1), tail(&stderr))),
        None => Err(format!("didn't print JSON{}", tail(if output.trim().is_empty() { &stderr } else { &output }))),
    }
}

/// Keeps a streaming plugin running until `stop`, reporting each line it prints, and restarting it
/// with a back-off when it exits.
pub fn stream(source: &Source, manifest: &Manifest, stop: &AtomicBool, mut report: impl FnMut(Result<Polled, String>)) {
    let Some(run) = manifest.run.as_ref() else { return report(Err("not a plugin".into())) };
    let input = settings(source, manifest).to_string();
    let mut backoff = FIRST_BACKOFF;
    while !stop.load(Ordering::Relaxed) {
        let mut child = match spawn(run, &input) {
            Ok(child) => child,
            Err(e) => {
                report(Err(e));
                if !pause(backoff, stop) {
                    return;
                }
                backoff = (backoff * 2).min(LONGEST_BACKOFF);
                continue;
            }
        };
        let (lines, errors) = readers(&mut child);
        loop {
            match lines.recv_timeout(Duration::from_millis(250)) {
                Ok(line) => {
                    let result = parse(&manifest.id, &line);
                    if result.is_ok() {
                        backoff = FIRST_BACKOFF; // it's working: a later crash gets a quick restart
                    }
                    report(result);
                }
                Err(RecvTimeoutError::Timeout) => {
                    if stop.load(Ordering::Relaxed) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break, // its stdout closed: it's going or gone
            }
        }
        let status = child.wait().ok();
        let stderr = errors.recv().unwrap_or_default();
        let code = status.and_then(|s| s.code()).unwrap_or(-1);
        report(Err(format!("exited with code {code}{}; restarting in {}", tail(&stderr), seconds(backoff))));
        if !pause(backoff, stop) {
            return;
        }
        backoff = (backoff * 2).min(LONGEST_BACKOFF);
    }
}

/// Starts the command in the plugin's folder, with no window, and feeds it `input` on stdin.
fn spawn(run: &Run, input: &str) -> Result<Child, String> {
    let mut command = Command::new(&run.command[0]);
    command.args(&run.command[1..]).current_dir(&run.dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|e| format!("couldn't run {:?}: {e}", run.command[0]))?;
    if let Some(mut stdin) = child.stdin.take() {
        // on its own thread: the settings may not fit the pipe before the plugin reads them
        let input = input.to_string();
        std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
            let _ = stdin.write_all(b"\n");
        });
    }
    Ok(child)
}

/// Reads the child's stdout line by line and its stderr's tail, each on a thread, so neither pipe
/// fills up while the other is waited on.
fn readers(child: &mut Child) -> (mpsc::Receiver<String>, mpsc::Receiver<String>) {
    let (lines, out) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout.take(MOST_OUTPUT));
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                if line.len() > LONGEST_LINE {
                    break;
                }
                if lines.send(line.trim_end_matches(['\r', '\n']).to_string()).is_err() {
                    break;
                }
                line.clear();
            }
            // whatever's left drains to nowhere, so the plugin never blocks on a full pipe
            let _ = std::io::copy(&mut reader.into_inner(), &mut std::io::sink());
        });
    }
    let (errors, err) = mpsc::channel();
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.take(MOST_OUTPUT).read_to_string(&mut text);
            let _ = errors.send(text);
        });
    }
    (out, err)
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
    let starts: Vec<usize> = output
        .match_indices('{')
        .map(|(i, _)| i)
        .filter(|&i| output[..i].rsplit('\n').next().unwrap_or("").trim().is_empty())
        .collect();
    starts.into_iter().rev().find_map(|i| serde_json::from_str(output[i..].trim()).ok())
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

/// The last part of what a plugin wrote, for an error message, on one line.
fn tail(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let start = text.char_indices().rev().nth(STDERR_TAIL).map_or(0, |(i, _)| i + 1);
    let shown: String = text[start..].lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" | ");
    format!(": {}{shown}", if start > 0 { "..." } else { "" })
}

/// One line of plugin output.
fn parse(id: &str, line: &str) -> Result<Polled, String> {
    let j: Json = serde_json::from_str(line).map_err(|_| format!("didn't print JSON{}", tail(line)))?;
    parse_json(id, &j)
}

/// `{"values": {...}}` becomes the values under `<id>.` that are numbers, text or ISO times; anything
/// else is noted and left out. `{"error": "..."}` is the plugin saying it failed.
fn parse_json(id: &str, j: &Json) -> Result<Polled, String> {
    if let Some(e) = j.get("error") {
        return Err(e.as_str().map_or_else(|| e.to_string(), |s| s.to_string()));
    }
    let given = j.get("values").and_then(Json::as_object).ok_or("the output has neither \"values\" nor \"error\"")?;
    let prefix = format!("{id}.");
    let (mut values, mut rejected) = (Vec::new(), Vec::new());
    for (key, v) in given {
        if !key.starts_with(&prefix) || key.len() == prefix.len() {
            rejected.push(format!("{key} isn't under {prefix}"));
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
            None => rejected.push(format!("{key} isn't a number, text or ISO time")),
        }
    }
    let mut notes: Vec<String> = rejected.iter().take(5).map(|r| format!("ignored {r}")).collect();
    if rejected.len() > 5 {
        notes.push(format!("and {} more ignored", rejected.len() - 5));
    }
    if values.is_empty() && !rejected.is_empty() {
        log(format_args!("data: {id}: {}", notes.join("; ")));
        return Err(format!("nothing usable: {}", notes.join("; ")));
    }
    Ok(Polled { values, notes })
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

    #[test]
    fn settings_with_defaults() {
        let f = Folder::new("settings");
        let m = f.plugin("w", "poll", "p.cmd", "");
        assert_eq!(settings(&source("w", 60.0), &m), json!({ "settings": { "city": "Oslo", "secret": "hunter2" } }));
        let given: Source = serde_json::from_value(json!({ "type": "w", "city": "Rome", "other": 1 })).unwrap();
        assert_eq!(settings(&given, &m)["settings"], json!({ "city": "Rome", "secret": null }));
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
        let stop = AtomicBool::new(false);
        let mut got: Vec<Result<Vec<(String, Value)>, String>> = Vec::new();
        let start = Instant::now();
        stream(&source("s", 60.0), &m, &stop, |r| {
            got.push(r.map(|p| p.values));
            // two runs: 2 values, an exit, a 1 s wait, 2 values, an exit
            if got.len() == 6 {
                stop.store(true, Ordering::Relaxed);
            }
        });
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

    #[test]
    fn parsing() {
        assert_eq!(parse("w", "{\"error\": \"x\"}").unwrap_err(), "x");
        assert_eq!(parse("w", "{\"error\": {\"code\": 1}}").unwrap_err(), "{\"code\":1}");
        assert_eq!(parse("w", "{}").unwrap_err(), "the output has neither \"values\" nor \"error\"");
        assert_eq!(parse("w", "nope").unwrap_err(), "didn't print JSON: nope");
        let p = parse("w", "{\"values\": {\"w.a\": 1, \"w.b\": true, \"w.c\": null, \"w.d\": \"2026-01-02T03:04:05+01:00\"}}").unwrap();
        assert_eq!(p.values.len(), 2);
        assert_eq!(p.notes.len(), 2);
        assert_eq!(tail("a\n\nb\n"), ": a | b");
        assert!(Path::new(".").exists());
    }
}
