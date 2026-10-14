//! Live data in text (docs/data.md): `{key}` placeholders in widget text, the values they're filled
//! from, and the sources in data.json (beside the playlist) that keep those values fresh, each polled on
//! a thread of its own.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread::sleep;
use std::time::{Duration, Instant};

use chrono::format::{Item as Spec, StrftimeItems};
use chrono::{DateTime, Local, SecondsFormat, TimeDelta, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::player::log;
use crate::sources::{self, Polled};
use crate::{api, playlist};

/// What a placeholder shows for a missing or stale value, when it gives no fallback.
const MISSING: &str = "--";
/// Values go stale after this many intervals without a successful update.
const STALE_AFTER: u32 = 3;
/// data.json is watched like the playlist: looked at this often, and read once it holds still.
const CHECK_EVERY: Duration = Duration::from_millis(500);
const SETTLE: Duration = Duration::from_millis(1500);
pub const DEFAULT_PORT: u16 = 8139;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Number(f64),
    Text(String),
    Time(DateTime<Utc>),
    /// Text counting down to a time, like `2h 34m`: worked out when it's shown, so it stays current.
    Until(DateTime<Utc>),
}

impl Value {
    /// A number when it is one (like Home Assistant's states, which are all strings), else text.
    pub fn guess(s: &str) -> Value {
        match s.parse::<f64>() {
            Ok(n) if n.is_finite() => Value::Number(n),
            _ => Value::Text(s.to_string()),
        }
    }

    /// For the API: numbers as numbers, times as ISO strings, the rest as text.
    pub fn json(&self, now: DateTime<Utc>) -> serde_json::Value {
        match self {
            Value::Number(n) => json!(n),
            Value::Text(s) => json!(s),
            Value::Time(t) => json!(iso(*t)),
            Value::Until(t) => json!(until(*t - now)),
        }
    }

    /// The value as text, formatted by `fmt` (empty for the default form). A format that doesn't suit
    /// the value is ignored.
    pub fn show(&self, fmt: &str, now: DateTime<Utc>) -> String {
        match self {
            Value::Number(n) => {
                let places = fmt.strip_prefix('.').and_then(|f| f.strip_suffix('f')).and_then(|p| p.parse::<usize>().ok());
                match (fmt, places) {
                    ("d", _) => format!("{:.0}", n.round() + 0.0), // + 0.0 turns -0 into 0
                    (_, Some(places)) => format!("{:.*}", places.min(20), n),
                    _ => {
                        let r = (n * 10.0).round() / 10.0 + 0.0;
                        if r.fract() == 0.0 { format!("{r:.0}") } else { format!("{r:.1}") }
                    }
                }
            }
            Value::Text(s) => s.clone(),
            Value::Time(t) => {
                let local = t.with_timezone(&Local);
                let pattern = if fmt.contains('%') && StrftimeItems::new(fmt).all(|i| i != Spec::Error) { fmt } else { "%H:%M" };
                let mut out = String::new();
                if write!(out, "{}", local.format(pattern)).is_err() {
                    out = local.format("%H:%M").to_string();
                }
                out
            }
            Value::Until(t) => until(*t - now),
        }
    }
}

pub fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Time left, in whole minutes rounded up: `3d 4h`, `2h 34m` or `45m`.
fn until(left: TimeDelta) -> String {
    let minutes = (left.num_seconds().max(0) + 59) / 60;
    let (d, h, m) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// A piece of widget text: as written, or a placeholder.
enum Part<'a> {
    Text(&'a str),
    Hole { key: &'a str, fmt: &'a str, fallback: Option<&'a str> },
}

/// Splits text into parts. `{{` and `}}` are literal braces; `{key:fmt|fallback}` is a placeholder (on
/// one line, with a key); any other brace is just a brace.
fn scan<'a>(text: &'a str, mut part: impl FnMut(Part<'a>)) {
    let mut rest = text;
    while let Some(at) = rest.find(['{', '}']) {
        part(Part::Text(&rest[..at]));
        let tail = &rest[at..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            part(Part::Text(&tail[..1]));
            rest = &tail[2..];
            continue;
        }
        if tail.starts_with('{') {
            let end = tail[1..].find(['{', '}', '\n']).filter(|&e| tail[1 + e..].starts_with('}'));
            if let Some(end) = end {
                let body = &tail[1..1 + end];
                let (spec, fallback) = match body.split_once('|') {
                    Some((spec, fallback)) => (spec, Some(fallback)),
                    None => (body, None),
                };
                let (key, fmt) = spec.split_once(':').unwrap_or((spec, ""));
                if !key.trim().is_empty() {
                    part(Part::Hole { key: key.trim(), fmt: fmt.trim(), fallback });
                    rest = &tail[end + 2..];
                    continue;
                }
            }
        }
        part(Part::Text(&tail[..1]));
        rest = &tail[1..];
    }
    part(Part::Text(rest));
}

/// Whether text has a placeholder, which makes its widget live.
pub fn is_live(text: &str) -> bool {
    let mut live = false;
    scan(text, |p| live |= matches!(p, Part::Hole { .. }));
    live
}

/// Text with its placeholders filled in from `lookup` (None for missing or stale) and its escaped
/// braces made plain.
pub fn fill(text: &str, lookup: impl Fn(&str) -> Option<Value>, now: DateTime<Utc>) -> String {
    let mut out = String::with_capacity(text.len());
    scan(text, |p| match p {
        Part::Text(s) => out.push_str(s),
        Part::Hole { key, fmt, fallback } => match lookup(key) {
            Some(value) => out.push_str(&value.show(fmt, now)),
            None => out.push_str(fallback.unwrap_or(MISSING)),
        },
    });
    out
}

/// Text filled in from the values as they are now. The one place text is formatted (the API's
/// /resolve uses it too), so the studio shows what the screen does.
pub fn resolve(text: &str) -> String {
    let now = Utc::now();
    fill(text, |key| store().get(key, now), now)
}

struct Entry {
    value: Value,
    at: DateTime<Utc>,
    source: String,
    stale_after: Duration,
}

impl Entry {
    fn stale(&self, now: DateTime<Utc>) -> bool {
        (now - self.at).to_std().is_ok_and(|age| age > self.stale_after)
    }
}

/// The latest value for each key, from whichever source gave it.
#[derive(Default)]
pub struct Store {
    entries: RwLock<HashMap<String, Entry>>,
}

/// The one store, filled by the source threads and read by the player and the API.
pub fn store() -> &'static Store {
    static STORE: OnceLock<Store> = OnceLock::new();
    STORE.get_or_init(Store::default)
}

impl Store {
    /// A successful poll's values, which stay fresh for 3 x `interval`.
    pub fn put(&self, source: &str, interval: Duration, values: Vec<(String, Value)>) {
        self.put_at(source, interval, values, Utc::now());
    }

    fn put_at(&self, source: &str, interval: Duration, values: Vec<(String, Value)>, at: DateTime<Utc>) {
        let mut entries = self.entries.write().unwrap();
        for (key, value) in values {
            entries.insert(key, Entry { value, at, source: source.to_string(), stale_after: interval * STALE_AFTER });
        }
    }

    /// A value, unless it's missing or stale. `time` is always there.
    pub fn get(&self, key: &str, now: DateTime<Utc>) -> Option<Value> {
        if key == "time" {
            return Some(Value::Time(now));
        }
        let entries = self.entries.read().unwrap();
        entries.get(key).filter(|e| !e.stale(now)).map(|e| e.value.clone())
    }

    /// Every value, sorted by key, with when it came, whether it's stale and its source.
    pub fn list(&self, now: DateTime<Utc>) -> Vec<(String, Value, DateTime<Utc>, bool, String)> {
        let entries = self.entries.read().unwrap();
        let mut list: Vec<_> = entries
            .iter()
            .map(|(k, e)| (k.clone(), e.value.clone(), e.at, e.stale(now), e.source.clone()))
            .chain([("time".to_string(), Value::Time(now), now, false, "time".to_string())])
            .collect();
        list.sort_by(|a, b| a.0.cmp(&b.0));
        list
    }
}

/// data.json. Anything left out takes its default.
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub api_port: u16,
    pub sources: Vec<Source>,
}

impl Default for Config {
    fn default() -> Config {
        Config { api_port: DEFAULT_PORT, sources: Vec::new() }
    }
}

/// One source in data.json. No Debug, so its token can't end up in a log by accident.
#[derive(Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Source {
    #[serde(rename = "type")]
    pub kind: String,
    pub enabled: bool,
    /// Seconds between polls.
    pub interval: f64,
    /// pc: read HWiNFO's shared memory too.
    pub hwinfo: bool,
    /// homeassistant
    pub url: String,
    pub token: String,
    pub entities: Vec<String>,
}

impl Default for Source {
    fn default() -> Source {
        Source { kind: String::new(), enabled: true, interval: 60.0, hwinfo: false, url: String::new(),
                 token: String::new(), entities: Vec::new() }
    }
}

impl Source {
    /// Time between polls: 60 s unless set, at least 5 s, and at least 30 s for Claude.
    pub fn every(&self) -> Duration {
        let least = if self.kind == "claude" { 30.0 } else { 5.0 };
        let secs = if self.interval.is_finite() { self.interval } else { 60.0 };
        Duration::from_secs_f64(secs.max(least))
    }
}

/// data.json lives beside the playlist.
pub fn file_for(playlist: &Path) -> PathBuf {
    playlist.parent().unwrap_or(Path::new(".")).join("data.json")
}

/// The sources set up in `file`; none if there's no file yet.
pub fn read_config(file: &Path) -> Result<Config, String> {
    match std::fs::read(file) {
        // only where it went wrong: serde's messages can quote values, and this file holds a token
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("{} isn't valid ({:?} at line {}, column {})", file.display(), e.classify(), e.line(), e.column())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("couldn't read {}: {e}", file.display())),
    }
}

/// Polls the sources in data.json into the store, each on its own thread, following edits to the file.
pub struct Hub {
    pub file: PathBuf,
    config: Mutex<Config>,
    /// The sources being polled, each with the flag that stops its thread.
    running: Mutex<Vec<(Source, Arc<AtomicBool>)>>,
    /// Each source's latest poll: its notes, or what went wrong.
    status: Mutex<BTreeMap<String, Result<Vec<String>, String>>>,
    /// The API's server, so a new port can take over from it.
    pub(crate) server: Mutex<Option<Arc<tiny_http::Server>>>,
}

/// Starts polling the sources in the data.json beside `playlist`, and serving the local API if `api`.
pub fn start(playlist: &Path, api: bool) -> Arc<Hub> {
    let file = file_for(playlist);
    let config = read_config(&file).unwrap_or_else(|e| {
        log(format_args!("data: {e}"));
        Config::default()
    });
    let hub = Arc::new(Hub { file, config: Mutex::default(), running: Mutex::default(), status: Mutex::default(),
                             server: Mutex::default() });
    hub.apply(config);
    let watcher = hub.clone();
    std::thread::spawn(move || watcher.watch());
    if api {
        let server = hub.clone();
        std::thread::spawn(move || api::serve(server));
    }
    hub
}

impl Hub {
    pub fn port(&self) -> u16 {
        self.config.lock().unwrap().api_port
    }

    /// Starts the sources that are new or changed, and stops the ones that changed or went.
    fn apply(self: &Arc<Self>, config: Config) {
        let mut running = self.running.lock().unwrap();
        let wanted: Vec<&Source> = config.sources.iter().filter(|s| s.enabled).collect();
        running.retain(|(source, stop)| {
            let keep = wanted.contains(&source);
            if !keep {
                stop.store(true, Ordering::Relaxed);
                self.status.lock().unwrap().remove(&source.kind);
            }
            keep
        });
        for source in wanted {
            if !running.iter().any(|(r, _)| r == source) {
                let stop = Arc::new(AtomicBool::new(false));
                let (hub, polled, flag) = (self.clone(), source.clone(), stop.clone());
                std::thread::spawn(move || hub.run(polled, flag));
                running.push((source.clone(), stop));
            }
        }
        let mut current = self.config.lock().unwrap();
        if current.api_port != config.api_port {
            // the API thread binds the new port once the old server stops
            if let Some(server) = self.server.lock().unwrap().take() {
                server.unblock();
            }
        }
        *current = config;
    }

    fn watch(self: Arc<Self>) {
        let mut seen = playlist::stamp(&self.file);
        let mut changing = None;
        loop {
            sleep(CHECK_EVERY);
            let found = playlist::stamp(&self.file);
            if found == seen {
                changing = None;
                continue;
            }
            match changing {
                Some((stamp, since)) if stamp == found => {
                    if Instant::now() - since < SETTLE {
                        continue;
                    }
                    changing = None;
                    seen = found;
                    match read_config(&self.file) {
                        Ok(config) => {
                            log(format_args!("data: read {}", self.file.display()));
                            self.apply(config);
                        }
                        Err(e) => log(format_args!("data: {e}, carrying on")),
                    }
                }
                _ => changing = Some((found, Instant::now())),
            }
        }
    }

    /// Polls one source at its interval until it's stopped. Failures are logged once, when they start
    /// or change, and its values just go stale.
    fn run(self: Arc<Self>, source: Source, stop: Arc<AtomicBool>) {
        let mut state = sources::State::default();
        let mut failing: Option<String> = None;
        while !stop.load(Ordering::Relaxed) {
            let result = sources::poll(&source, &mut state);
            if stop.load(Ordering::Relaxed) {
                return; // replaced meanwhile
            }
            match result {
                Ok(Polled { values, notes }) => {
                    store().put(&source.kind, source.every(), values);
                    if failing.take().is_some() {
                        log(format_args!("data: {} is working again", source.kind));
                    }
                    self.status.lock().unwrap().insert(source.kind.clone(), Ok(notes));
                }
                Err(e) => {
                    if failing.as_ref() != Some(&e) {
                        log(format_args!("data: {}: {e}", source.kind));
                    }
                    self.status.lock().unwrap().insert(source.kind.clone(), Err(e.clone()));
                    failing = Some(e);
                }
            }
            let next = Instant::now() + source.every();
            while Instant::now() < next && !stop.load(Ordering::Relaxed) {
                sleep(Duration::from_millis(250));
            }
        }
    }

    /// Runs a source once without saving it (the API's /test). The browser never gets Home Assistant's
    /// token, so a test without one uses the saved one.
    pub fn test(&self, mut source: Source) -> Result<Polled, String> {
        if source.kind == "homeassistant" && source.token.is_empty() {
            let config = self.config.lock().unwrap();
            if let Some(saved) = config.sources.iter().find(|s| s.kind == "homeassistant") {
                source.token = saved.token.clone();
            }
        }
        sources::poll(&source, &mut sources::State::default())
    }

    /// Waits until every source has been polled once, or `timeout` passes.
    pub fn wait(&self, timeout: Duration) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            let kinds: Vec<String> = self.running.lock().unwrap().iter().map(|(s, _)| s.kind.clone()).collect();
            let status = self.status.lock().unwrap();
            if kinds.iter().all(|k| status.contains_key(k)) {
                return;
            }
            drop(status);
            sleep(Duration::from_millis(100));
        }
    }

    /// How each source did, then every value in its default form.
    pub fn report(&self) -> String {
        let mut out = String::new();
        if !self.file.exists() {
            let _ = writeln!(out, "no {} yet: only the time", self.file.display());
        }
        for (kind, status) in self.status.lock().unwrap().iter() {
            match status {
                Ok(notes) if notes.is_empty() => _ = writeln!(out, "{kind}: ok"),
                Ok(notes) => _ = writeln!(out, "{kind}: ok ({})", notes.join("; ")),
                Err(e) => _ = writeln!(out, "{kind}: failed: {e}"),
            }
        }
        let now = Utc::now();
        for (key, value, _, stale, source) in store().list(now) {
            let _ = writeln!(out, "  {key} = {}{}  [{source}]", value.show("", now), if stale { " (stale)" } else { "" });
        }
        out
    }
}

/// Fills in texts through the running player's API (the values it has now), for `wvplay render`. None
/// when no player is answering.
pub fn ask_player(playlist: &Path, texts: &[String]) -> Option<Vec<String>> {
    let port = read_config(&file_for(playlist)).ok()?.api_port;
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(3))).build().into();
    let body = json!({ "texts": texts }).to_string();
    let reply = agent.post(format!("http://127.0.0.1:{port}/resolve")).send(body).ok()?.body_mut().read_to_string().ok()?;
    let reply: serde_json::Value = serde_json::from_str(&reply).ok()?;
    let filled: Option<Vec<String>> = reply["texts"].as_array()?.iter().map(|t| t.as_str().map(String::from)).collect();
    filled.filter(|f| f.len() == texts.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn values() -> impl Fn(&str) -> Option<Value> {
        |key| match key {
            "n.whole" => Some(Value::Number(30.0)),
            "n.frac" => Some(Value::Number(94.25)),
            "n.small" => Some(Value::Number(2.04)),
            "n.neg" => Some(Value::Number(-0.3)),
            "n.half" => Some(Value::Number(2.5)),
            "t.text" => Some(Value::Text("critical".into())),
            "t.at" => Some(Value::Time(Utc.with_ymd_and_hms(2026, 10, 14, 10, 50, 0).unwrap())),
            "t.in" => Some(Value::Until(Utc.with_ymd_and_hms(2026, 10, 14, 12, 34, 30).unwrap())),
            "t.week" => Some(Value::Until(Utc.with_ymd_and_hms(2026, 10, 17, 14, 0, 0).unwrap())),
            _ => None,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 14, 10, 0, 0).unwrap()
    }

    fn f(text: &str) -> String {
        fill(text, values(), now())
    }

    #[test]
    fn placeholders() {
        assert_eq!(f("CPU {n.whole}%"), "CPU 30%");
        assert_eq!(f("{n.whole}{n.frac}"), "3094.3");
        // escapes, and braces that aren't placeholders
        assert_eq!(f("{{n.whole}} = {n.whole}"), "{n.whole} = 30");
        assert_eq!(f("{{{n.whole}}}"), "{30}");
        assert_eq!(f("a { b"), "a { b");
        assert_eq!(f("a } b {} {:d} { }"), "a } b {} {:d} { }");
        assert_eq!(f("{n.whole\n}"), "{n.whole\n}");
        assert_eq!(f("{ n.whole }"), "30");
        // which widgets are live
        assert!(is_live("x {time}"));
        assert!(!is_live("x {{time}}"));
        assert!(!is_live("plain {"));
        assert!(!is_live(""));
    }

    #[test]
    fn fallbacks() {
        assert_eq!(f("{nope}"), "--");
        assert_eq!(f("{nope|n/a}"), "n/a");
        assert_eq!(f("{nope:d|?}"), "?");
        assert_eq!(f("{nope|}"), "");
        assert_eq!(f("{nope|a:b|c}"), "a:b|c");
        assert_eq!(f("{n.whole:d|?}"), "30");
    }

    #[test]
    fn numbers() {
        assert_eq!(f("{n.frac}"), "94.3");
        assert_eq!(f("{n.small}"), "2"); // 2.0 rounds to a whole number: no ".0"
        assert_eq!(f("{n.neg}"), "-0.3");
        assert_eq!(f("{n.frac:d} {n.half:d} {n.neg:d}"), "94 3 0");
        assert_eq!(f("{n.frac:.2f} {n.whole:.1f} {n.frac:.0f}"), "94.25 30.0 94");
        // a time format on a number, and nonsense, are ignored
        assert_eq!(f("{n.frac:%H:%M} {n.frac:x} {n.frac:.f}"), "94.3 94.3 94.3");
    }

    #[test]
    fn text_and_times() {
        assert_eq!(f("{t.text} {t.text:d} {t.text:.1f}"), "critical critical critical");
        let at = Utc.with_ymd_and_hms(2026, 10, 14, 10, 50, 0).unwrap().with_timezone(&Local);
        assert_eq!(f("{t.at}"), at.format("%H:%M").to_string());
        assert_eq!(f("{t.at:%a %d %b}"), at.format("%a %d %b").to_string());
        assert_eq!(f("{t.at:%H:%M:%S}"), at.format("%H:%M:%S").to_string());
        // a numeric format on a time, and a broken pattern, give the default
        assert_eq!(f("{t.at:d}"), at.format("%H:%M").to_string());
        assert_eq!(f("{t.at:%Q}"), at.format("%H:%M").to_string());
        // countdowns, in whole minutes rounded up
        assert_eq!(f("{t.in}"), "2h 35m");
        assert_eq!(f("{t.week}"), "3d 4h");
        assert_eq!(until(TimeDelta::seconds(45 * 60)), "45m");
        assert_eq!(until(TimeDelta::seconds(-5)), "0m");
        // time is built in
        let store = Store::default();
        assert_eq!(store.get("time", now()), Some(Value::Time(now())));
    }

    #[test]
    fn staleness() {
        let store = Store::default();
        let minute = Duration::from_secs(60);
        store.put_at("pc", minute, vec![("pc.cpu.load".into(), Value::Number(12.0))], now());
        store.put_at("pc", minute, vec![("pc.old".into(), Value::Number(1.0))], now() - TimeDelta::seconds(181));
        let lookup = |key: &str| store.get(key, now());
        assert_eq!(fill("{pc.cpu.load:d}% {pc.old} {pc.old|stale}", lookup, now()), "12% -- stale");
        // fresh until 3 x interval
        assert!(store.get("pc.cpu.load", now() + TimeDelta::seconds(180)).is_some());
        assert!(store.get("pc.cpu.load", now() + TimeDelta::seconds(181)).is_none());
        let listed: Vec<(String, bool)> = store.list(now()).into_iter().map(|(k, _, _, stale, _)| (k, stale)).collect();
        assert_eq!(listed, [("pc.cpu.load".into(), false), ("pc.old".into(), true), ("time".into(), false)]);
    }

    #[test]
    fn config() {
        let c: Config = serde_json::from_str(r#"{ "sources": [ { "type": "claude", "interval": 10 },
            { "type": "pc", "interval": 1, "hwinfo": true, "enabled": false }, { "type": "homeassistant" } ] }"#).unwrap();
        assert_eq!(c.api_port, DEFAULT_PORT);
        let every: Vec<u64> = c.sources.iter().map(|s| s.every().as_secs()).collect();
        assert_eq!(every, [30, 5, 60]);
        assert!(c.sources[0].enabled && !c.sources[1].enabled && c.sources[1].hwinfo);
    }
}
