//! Live data in text (docs/data.md): `{key}` placeholders in text objects, the values they're filled
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
use crate::{api, catalog, playlist, plugin};

/// What a placeholder shows for a missing or stale value, when it gives no fallback.
const MISSING: &str = "--";
/// Values go stale after this many intervals without a successful update.
const STALE_AFTER: u32 = 3;
/// The most keys a plugin's source can have: nothing removes a key, so one that made up a new key for
/// each event would otherwise grow the player, and last-values.json, without end.
const MOST_KEYS: usize = 1000;
/// Claude's usage changes slowly, and its endpoint rate-limits: its values last this long instead.
const CLAUDE_STALE_AFTER: Duration = Duration::from_secs(60 * 60);
/// A source that fails (Claude), or is told to wait (any source's 429 or 503), is tried again after
/// this long, doubling each time up to the cap, unless `Retry-After` says otherwise.
const BACKOFF_FIRST: Duration = Duration::from_secs(2 * 60);
const BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);
/// The most a `Retry-After` is honoured for.
const RETRY_AFTER_MAX: Duration = Duration::from_secs(60 * 60);
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

    /// For last-values.json: numbers and text as they are, times tagged so they come back as times.
    fn to_saved(&self) -> serde_json::Value {
        match self {
            Value::Number(n) => json!(n),
            Value::Text(s) => json!(s),
            Value::Time(t) => json!({ "time": iso(*t) }),
            Value::Until(t) => json!({ "until": iso(*t) }),
        }
    }

    fn from_saved(j: &serde_json::Value) -> Option<Value> {
        let time = |j: &serde_json::Value| j.as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&Utc));
        match j {
            serde_json::Value::Number(n) => n.as_f64().map(Value::Number),
            serde_json::Value::String(s) => Some(Value::Text(s.clone())),
            serde_json::Value::Object(o) => match (o.get("time"), o.get("until")) {
                (Some(t), _) => time(t).map(Value::Time),
                (_, Some(t)) => time(t).map(Value::Until),
                _ => None,
            },
            _ => None,
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

/// A piece of text: as written, or a placeholder.
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

/// Whether text has a placeholder, which makes it live.
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

/// `resolve`, plus which of the text's placeholders, in order, had no value (and so showed their fallback or
/// the missing-value text): the player draws a value at once when one of these gets one (docs/data.md).
pub fn resolve_gaps(text: &str) -> (String, Vec<bool>) {
    let now = Utc::now();
    let gaps = std::cell::RefCell::new(Vec::new());
    let filled = fill(text, |key| {
        let value = store().get(key, now);
        gaps.borrow_mut().push(value.is_none());
        value
    }, now);
    (filled, gaps.into_inner())
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
    /// A successful poll's values, which stay fresh for `stale_after` (see `Source::stale_after`).
    /// Returns how many new keys were refused because a plugin's source has MOST_KEYS already.
    pub fn put(&self, source: &str, stale_after: Duration, values: Vec<(String, Value)>) -> usize {
        self.put_at(source, stale_after, values, Utc::now())
    }

    fn put_at(&self, source: &str, stale_after: Duration, values: Vec<(String, Value)>, at: DateTime<Utc>) -> usize {
        // a built-in's keys are bounded by what it reads; a plugin could make up a new one every time
        let most = if catalog::is_builtin(source) { usize::MAX } else { MOST_KEYS };
        let mut entries = self.entries.write().unwrap();
        let (mut held, mut refused) = (None, 0);
        for (key, value) in values {
            if let Some(e) = entries.get_mut(&key).filter(|e| e.source == source) {
                (e.value, e.at, e.stale_after) = (value, at, stale_after);
                continue;
            }
            // a new key: counted against the source's keys, which are counted once, at its first
            let held = held.get_or_insert_with(|| entries.values().filter(|e| e.source == source).count());
            if *held >= most {
                refused += 1;
                continue;
            }
            *held += 1;
            entries.insert(key, Entry { value, at, source: source.to_string(), stale_after });
        }
        refused
    }

    /// Writes every source's latest values and their time to `file` (last-values.json), so they're
    /// there again after a restart. Values only: no settings, and never a token.
    pub fn save(&self, file: &Path) -> std::io::Result<()> {
        let mut sources: BTreeMap<String, (DateTime<Utc>, serde_json::Map<String, serde_json::Value>)> = BTreeMap::new();
        for (key, e) in self.entries.read().unwrap().iter() {
            let (at, values) = sources.entry(e.source.clone()).or_insert_with(|| (e.at, serde_json::Map::new()));
            *at = (*at).max(e.at);
            values.insert(key.clone(), e.value.to_saved());
        }
        let json: serde_json::Map<String, serde_json::Value> =
            sources.into_iter().map(|(source, (at, values))| (source, json!({ "at": iso(at), "values": values }))).collect();
        if let Some(folder) = file.parent() {
            std::fs::create_dir_all(folder)?;
        }
        // written whole, then put in place, so a reader never sees half a file
        let partial = file.with_extension("json.part");
        std::fs::write(&partial, serde_json::to_vec(&json)?)?;
        std::fs::rename(&partial, file)
    }

    /// Reads a saved last-values.json back, keeping each value's real time, so staleness still applies.
    /// `stale_after` gives a source's rule, or None to leave that source's values out. Returns when
    /// each loaded source last succeeded.
    pub fn load(&self, file: &Path, stale_after: impl Fn(&str) -> Option<Duration>) -> Vec<(String, DateTime<Utc>)> {
        let Ok(bytes) = std::fs::read(file) else { return Vec::new() };
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return Vec::new() };
        let mut loaded = Vec::new();
        for (source, saved) in json.as_object().into_iter().flatten() {
            let Some(window) = stale_after(source) else { continue };
            let Some(at) = saved["at"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()) else { continue };
            let at = at.with_timezone(&Utc);
            let values: Vec<(String, Value)> = saved["values"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(key, v)| Value::from_saved(v).map(|v| (key.clone(), v)))
                .collect();
            if values.is_empty() {
                continue;
            }
            self.put_at(source, window, values, at);
            loaded.push((source.clone(), at));
        }
        loaded
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
///
/// Its settings sit beside `type`, `enabled` and `interval`. A built-in's are read into the typed fields
/// below; a plugin's all go in `extra`, whatever their keys, so a plugin's `url` or `hwinfo` is its own.
#[derive(Clone, PartialEq)]
pub struct Source {
    pub kind: String,
    pub enabled: bool,
    /// Seconds between polls; the manifest's default when left out.
    pub interval: Option<f64>,
    /// pc: read HWiNFO's shared memory too.
    pub hwinfo: bool,
    /// pc: read Libre Hardware Monitor's web server too, at `lhm_url`, with the user name and password
    /// its Authentication was given, when either is set.
    pub lhm: bool,
    pub lhm_url: String,
    pub lhm_user: String,
    pub lhm_password: String,
    /// homeassistant
    pub url: String,
    pub token: String,
    pub entities: Vec<String>,
    /// A plugin's settings, whatever its manifest names.
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// What every source has, and the rest of its keys.
#[derive(Deserialize)]
#[serde(default)]
struct Common {
    #[serde(rename = "type")]
    kind: String,
    enabled: bool,
    interval: Option<f64>,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

impl Default for Common {
    fn default() -> Common {
        Common { kind: String::new(), enabled: true, interval: None, rest: serde_json::Map::new() }
    }
}

/// The built-ins' own settings, with their types.
#[derive(Deserialize)]
#[serde(default)]
struct Builtin {
    hwinfo: bool,
    lhm: bool,
    lhm_url: String,
    lhm_user: String,
    lhm_password: String,
    url: String,
    token: String,
    entities: Vec<String>,
}

impl Default for Builtin {
    fn default() -> Builtin {
        let s = Source::default();
        Builtin { hwinfo: s.hwinfo, lhm: s.lhm, lhm_url: s.lhm_url, lhm_user: s.lhm_user, lhm_password: s.lhm_password,
                  url: s.url, token: s.token, entities: s.entities }
    }
}

const BUILTIN_KEYS: [&str; 8] = ["hwinfo", "lhm", "lhm_url", "lhm_user", "lhm_password", "url", "token", "entities"];

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Source, D::Error> {
        let Common { kind, enabled, interval, mut rest } = Common::deserialize(d)?;
        let mut source = Source { kind, enabled, interval, ..Source::default() };
        if catalog::is_builtin(&source.kind) {
            let own: serde_json::Map<_, _> = BUILTIN_KEYS.iter().filter_map(|k| rest.remove_entry(*k)).collect();
            let b: Builtin = serde_json::from_value(serde_json::Value::Object(own)).map_err(serde::de::Error::custom)?;
            (source.hwinfo, source.lhm, source.lhm_url) = (b.hwinfo, b.lhm, b.lhm_url);
            (source.lhm_user, source.lhm_password) = (b.lhm_user, b.lhm_password);
            (source.url, source.token, source.entities) = (b.url, b.token, b.entities);
        }
        source.extra = rest;
        Ok(source)
    }
}

impl Default for Source {
    fn default() -> Source {
        Source { kind: String::new(), enabled: true, interval: None, hwinfo: false, lhm: false,
                 lhm_url: sources::LHM_URL.to_string(), lhm_user: String::new(), lhm_password: String::new(),
                 url: String::new(), token: String::new(), entities: Vec::new(), extra: serde_json::Map::new() }
    }
}

impl Source {
    /// Time between polls: the manifest's default unless set, never under its minimum (60 s for
    /// Claude, 5 s otherwise), and never over a day, however large a number data.json holds.
    pub fn every(&self) -> Duration {
        let spec = catalog::manifest(&self.kind).map(|m| m.interval).unwrap_or_default();
        let secs = self.interval.filter(|i| i.is_finite()).unwrap_or(spec.default);
        Duration::from_secs_f64(secs.max(spec.min).max(1.0).min(catalog::LONGEST_INTERVAL))
    }

    /// How long this source's values stay fresh without a successful poll: 3 x interval, except
    /// Claude's, which last an hour (usage changes slowly, and a back-off shouldn't blank the text).
    pub fn stale_after(&self) -> Duration {
        if self.kind == "claude" { CLAUDE_STALE_AFTER } else { self.every().saturating_mul(STALE_AFTER) }
    }

    /// Whether a failure makes this source back off: Claude on any error (its endpoint rate-limits a
    /// login that polls too much), every source when the server says to wait.
    fn backs_off(&self, failure: &sources::Failure) -> bool {
        self.kind == "claude" || failure.throttled()
    }

    /// A setting as it is in data.json, by its manifest key, or None when it isn't set.
    pub fn setting(&self, key: &str) -> Option<serde_json::Value> {
        if !catalog::is_builtin(&self.kind) {
            return self.extra.get(key).cloned();
        }
        match key {
            "hwinfo" => Some(json!(self.hwinfo)),
            "lhm" => Some(json!(self.lhm)),
            "lhm_url" => Some(json!(self.lhm_url)).filter(|_| !self.lhm_url.is_empty()),
            "lhm_user" => Some(json!(self.lhm_user)).filter(|_| !self.lhm_user.is_empty()),
            "lhm_password" => Some(json!(self.lhm_password)).filter(|_| !self.lhm_password.is_empty()),
            "url" => Some(json!(self.url)).filter(|_| !self.url.is_empty()),
            "token" => Some(json!(self.token)).filter(|_| !self.token.is_empty()),
            "entities" => Some(json!(self.entities)).filter(|_| !self.entities.is_empty()),
            _ => self.extra.get(key).cloned(),
        }
    }

    /// Whether a password setting is blank, so the saved one should stand in (/test).
    fn lacks_password(&self, key: &str) -> bool {
        self.setting(key).is_none_or(|v| v.as_str().is_none_or(str::is_empty))
    }

    fn copy_setting(&mut self, key: &str, from: &Source) {
        match key {
            "token" if catalog::is_builtin(&self.kind) => self.token = from.token.clone(),
            "lhm_password" if catalog::is_builtin(&self.kind) => self.lhm_password = from.lhm_password.clone(),
            "url" if catalog::is_builtin(&self.kind) => self.url = from.url.clone(),
            "hwinfo" | "lhm" | "lhm_url" | "lhm_user" | "entities" if catalog::is_builtin(&self.kind) => {}
            _ => {
                if let Some(v) = from.extra.get(key) {
                    self.extra.insert(key.to_string(), v.clone());
                }
            }
        }
    }
}

/// data.json lives beside the playlist.
pub fn file_for(playlist: &Path) -> PathBuf {
    playlist.parent().unwrap_or(Path::new(".")).join("data.json")
}

/// Plugins live in `plugins/`, next to the playlist's folder.
pub fn plugins_for(playlist: &Path) -> PathBuf {
    let folder = playlist.parent().unwrap_or(Path::new("."));
    folder.parent().unwrap_or(folder).join("plugins")
}

/// The last good values live in %LOCALAPPDATA%\wvplay\last-values.json.
#[cfg(not(test))]
pub fn last_values_file() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, PathBuf::from).join("wvplay").join("last-values.json")
}

/// Tests keep theirs in a file of their own, never the running player's.
#[cfg(test)]
pub fn last_values_file() -> PathBuf {
    std::env::temp_dir().join(format!("wvplay-test-last-values-{}.json", std::process::id()))
}

/// How long to wait before a source's first poll: none, unless its last success (from the last run)
/// is younger than its interval, when the remainder is waited out.
pub fn first_poll_delay(last_success: Option<DateTime<Utc>>, every: Duration, now: DateTime<Utc>) -> Duration {
    match last_success.and_then(|at| (now - at).to_std().ok()) {
        Some(age) => every.saturating_sub(age),
        None => Duration::ZERO,
    }
}

/// A source's back-off: how long to wait after each failure in a row, doubling from 2 min to 30 min,
/// unless the server said how long (`Retry-After`). A success resets it.
#[derive(Default, Debug)]
pub struct Backoff {
    failures: u32,
}

impl Backoff {
    /// Notes a failure and gives the wait before the next try.
    pub fn next(&mut self, retry_after: Option<Duration>) -> Duration {
        self.failures = self.failures.saturating_add(1);
        match retry_after {
            Some(asked) => asked.min(RETRY_AFTER_MAX),
            None => BACKOFF_FIRST.saturating_mul(1u32 << (self.failures - 1).min(16)).min(BACKOFF_MAX),
        }
    }

    pub fn reset(&mut self) {
        self.failures = 0;
    }

    pub fn active(&self) -> bool {
        self.failures > 0
    }
}

/// A wait, briefly: `45s`, `2m`, `1h 30m`.
fn brief(d: Duration) -> String {
    let secs = d.as_secs();
    match (secs / 3600, secs / 60 % 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

/// How a source's last poll went, for the catalog and the report.
#[derive(Clone)]
pub struct Status {
    pub result: Result<Vec<String>, String>,
    /// When it last gave values.
    pub updated_at: Option<DateTime<Utc>>,
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
    status: Mutex<BTreeMap<String, Status>>,
    /// The API's server, so a new port can take over from it.
    pub(crate) server: Mutex<Option<Arc<tiny_http::Server>>>,
    /// Where plugins are read from, at start-up and on rescan.
    pub plugins: PathBuf,
    /// When last-values.json was last written, so a streaming plugin doesn't rewrite it on every line.
    saved_at: Mutex<Option<Instant>>,
}

/// Starts polling the sources in the data.json beside `playlist`, and serving the local API if `api`.
pub fn start(playlist: &Path, api: bool) -> Arc<Hub> {
    let file = file_for(playlist);
    let config = read_config(&file).unwrap_or_else(|e| {
        log(format_args!("data: {e}"));
        Config::default()
    });
    let hub = Arc::new(Hub { file, config: Mutex::default(), running: Mutex::default(), status: Mutex::default(),
                             server: Mutex::default(), plugins: plugins_for(playlist), saved_at: Mutex::default() });
    let found = catalog::rescan(&hub.plugins);
    let plugins = found.manifests.len() - catalog::builtins().len();
    if plugins > 0 || hub.plugins.is_dir() {
        log(format_args!("plugins: {plugins} in {}", hub.plugins.display()));
    }
    // the last run's values, with their real age, for the sources still set up
    let stale_after = |kind: &str| config.sources.iter().find(|s| s.kind == kind && s.enabled).map(Source::stale_after);
    let loaded = store().load(&last_values_file(), stale_after);
    if !loaded.is_empty() {
        let mut status = hub.status.lock().unwrap();
        for (kind, at) in loaded {
            status.insert(kind, Status { result: Ok(Vec::new()), updated_at: Some(at) });
        }
    }
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
    /// or change, and its values just go stale; a failure that backs off (see `Source::backs_off`)
    /// pushes the next poll out. A streaming plugin is kept running instead.
    fn run(self: Arc<Self>, source: Source, stop: Arc<AtomicBool>) {
        let mut failing: Option<String> = None;
        let mut backoff = Backoff::default();
        if let Some(manifest) = catalog::manifest(&source.kind).filter(|m| m.streams()) {
            return plugin::stream(&source, &manifest, &stop, |result| {
                if !stop.load(Ordering::Relaxed) {
                    self.record(&source, result.map_err(sources::Failure::from), &mut failing, &mut backoff, true);
                }
            });
        }
        let mut state = sources::State::default();
        // values from the last run that are still fresh wait out the rest of the interval
        let last_success = self.status.lock().unwrap().get(&source.kind).and_then(|s| s.updated_at);
        let mut next = Instant::now() + first_poll_delay(last_success, source.every(), Utc::now());
        while !stop.load(Ordering::Relaxed) {
            if Instant::now() < next {
                sleep(Duration::from_millis(250));
                continue;
            }
            let result = sources::poll(&source, &mut state);
            if stop.load(Ordering::Relaxed) {
                return; // replaced meanwhile
            }
            let wait = self.record(&source, result, &mut failing, &mut backoff, false);
            next = Instant::now() + wait;
        }
    }

    /// Keeps a poll's values and status, and gives the wait before the next poll: the interval, or
    /// longer while backing off. `failing` is the error being logged, so each is logged once; while
    /// backing off, only the start and the recovery are logged. A `streaming` source never backs off
    /// (nothing polls it: it keeps running, and waiting is up to it), and its values are saved for the
    /// next run at most every 10 s.
    fn record(&self, source: &Source, result: Result<Polled, sources::Failure>, failing: &mut Option<String>,
              backoff: &mut Backoff, streaming: bool) -> Duration {
        let mut status = self.status.lock().unwrap();
        let updated_at = status.get(&source.kind).and_then(|s| s.updated_at);
        match result {
            Ok(Polled { values, mut notes }) => {
                let now = Utc::now();
                let refused = store().put(source.kind.as_str(), source.stale_after(), values);
                if refused > 0 {
                    notes.push(format!("ignored {refused} new key{}: a plugin can set {MOST_KEYS} keys at most",
                                       if refused == 1 { "" } else { "s" }));
                }
                let mut saved_at = self.saved_at.lock().unwrap();
                if !streaming || saved_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(10)) {
                    let _ = store().save(&last_values_file());
                    *saved_at = Some(Instant::now());
                }
                if failing.take().is_some() {
                    log(format_args!("data: {} is working again", source.kind));
                }
                backoff.reset();
                status.insert(source.kind.clone(), Status { result: Ok(notes), updated_at: Some(now) });
                source.every()
            }
            Err(e) => {
                let wait = if !streaming && source.backs_off(&e) {
                    let was_backing_off = backoff.active();
                    let wait = backoff.next(e.retry_after).max(source.every());
                    if !was_backing_off {
                        log(format_args!("data: {}: {e}; backing off, trying again in {}", source.kind, brief(wait)));
                    }
                    wait
                } else {
                    if failing.as_ref() != Some(&e.message) {
                        log(format_args!("data: {}: {e}", source.kind));
                    }
                    source.every()
                };
                status.insert(source.kind.clone(), Status { result: Err(e.message.clone()), updated_at });
                *failing = Some(e.message);
                wait
            }
        }
    }

    /// Runs a source once without saving it (the API's /test). The browser never gets a password
    /// setting back, so a test without one uses the saved one.
    pub fn test(&self, mut source: Source) -> Result<Polled, String> {
        if let Some(manifest) = catalog::manifest(&source.kind) {
            let config = self.config.lock().unwrap();
            // A saved password only stands in while every text setting (an address, a host, a user
            // name: lhm_url and lhm_user for LHM's) is as saved: whoever can reach the studio could
            // otherwise point a source at their own server.
            let same = |saved: &Source| {
                let norm = |v: Option<serde_json::Value>| {
                    v.and_then(|v| v.as_str().map(|s| s.trim().trim_end_matches('/').to_lowercase()))
                };
                manifest.settings.iter().filter(|s| s.kind == "text")
                    .all(|s| norm(source.setting(&s.key)) == norm(saved.setting(&s.key)))
            };
            if let Some(saved) = config.sources.iter().find(|s| s.kind == source.kind).filter(|s| same(s)) {
                for s in manifest.settings.iter().filter(|s| s.kind == "password") {
                    if source.lacks_password(&s.key) {
                        source.copy_setting(&s.key, saved);
                    }
                }
            }
        }
        sources::poll(&source, &mut sources::State::default()).map_err(|e| e.message)
    }

    /// Re-reads the plugins folder, and restarts the plugins that are on, so a changed command or
    /// mode takes. Returns the new catalog.
    pub fn rescan(self: &Arc<Self>) -> catalog::Catalog {
        let found = catalog::rescan(&self.plugins);
        let builtin = |kind: &str| catalog::builtins().iter().any(|m| m.id == kind);
        let mut running = self.running.lock().unwrap();
        running.retain(|(source, stop)| {
            let keep = builtin(&source.kind);
            if !keep {
                stop.store(true, Ordering::Relaxed);
                self.status.lock().unwrap().remove(&source.kind);
            }
            keep
        });
        drop(running);
        let config = self.config.lock().unwrap().clone();
        self.apply(config);
        found
    }

    /// The catalog as the API gives it: each manifest, without `run`, with its status here.
    pub fn catalog(&self) -> serde_json::Value {
        let config = self.config.lock().unwrap().clone();
        let status = self.status.lock().unwrap().clone();
        catalog::current().json(|m| {
            let enabled = m.id == "time" || config.sources.iter().any(|s| s.kind == m.id && s.enabled);
            let (ok, error, notes, updated_at) = match (m.id.as_str(), status.get(&m.id)) {
                ("time", _) => (true, None, Vec::new(), Some(iso(Utc::now()))),
                (_, Some(s)) => match &s.result {
                    Ok(notes) => (true, None, notes.clone(), s.updated_at.map(iso)),
                    Err(e) => (false, Some(e.clone()), Vec::new(), s.updated_at.map(iso)),
                },
                // on, but not polled yet; or off
                (_, None) => (false, None, Vec::new(), None),
            };
            json!({ "enabled": enabled, "ok": ok, "error": error, "notes": notes, "updated_at": updated_at })
        })
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
            match &status.result {
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
        // which text is live
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
        let window = source("pc", Some(60.0)).stale_after(); // 3 x interval
        store.put_at("pc", window, vec![("pc.cpu.load".into(), Value::Number(12.0))], now());
        store.put_at("pc", window, vec![("pc.old".into(), Value::Number(1.0))], now() - TimeDelta::seconds(181));
        let lookup = |key: &str| store.get(key, now());
        assert_eq!(fill("{pc.cpu.load:d}% {pc.old} {pc.old|stale}", lookup, now()), "12% -- stale");
        // fresh until 3 x interval
        assert!(store.get("pc.cpu.load", now() + TimeDelta::seconds(180)).is_some());
        assert!(store.get("pc.cpu.load", now() + TimeDelta::seconds(181)).is_none());
        let listed: Vec<(String, bool)> = store.list(now()).into_iter().map(|(k, _, _, stale, _)| (k, stale)).collect();
        assert_eq!(listed, [("pc.cpu.load".into(), false), ("pc.old".into(), true), ("time".into(), false)]);
    }

    fn source(kind: &str, interval: Option<f64>) -> Source {
        Source { kind: kind.into(), interval, ..Source::default() }
    }

    /// However large an interval data.json holds, it's a day at most, so working out a wait or a
    /// staleness never overflows.
    #[test]
    fn huge_intervals() {
        for interval in [86_401.0, 1e19, 1e20, 1e300, f64::MAX] {
            let s = source("weather", Some(interval));
            assert_eq!(s.every(), Duration::from_secs(86_400), "{interval}");
            assert_eq!(s.stale_after(), Duration::from_secs(3 * 86_400), "{interval}");
        }
        assert_eq!(source("weather", Some(f64::INFINITY)).every(), Duration::from_secs(60));
        assert_eq!(source("weather", Some(-1e300)).every(), Duration::from_secs(5));
    }

    /// A plugin's source keeps MOST_KEYS keys at most: the ones it has still update, and new ones past
    /// that are refused. Each source counts its own, and a built-in's aren't limited.
    #[test]
    fn keys_per_source() {
        let store = Store::default();
        let window = Duration::from_secs(60);
        let keys = |from: usize, n: usize, v: f64| (from..from + n).map(|i| (format!("p.k{i}"), Value::Number(v))).collect::<Vec<_>>();
        assert_eq!(store.put_at("p", window, keys(0, 990, 1.0), now()), 0);
        // 10 known, 10 new, then 10 over the limit
        assert_eq!(store.put_at("p", window, keys(980, 30, 2.0), now()), 10);
        assert_eq!(store.get("p.k985", now()), Some(Value::Number(2.0)));
        assert_eq!(store.get("p.k999", now()), Some(Value::Number(2.0)));
        assert_eq!(store.get("p.k1000", now()), None);
        assert_eq!(store.put_at("p", window, keys(0, MOST_KEYS, 3.0), now()), 0);
        assert_eq!(store.get("p.k0", now()), Some(Value::Number(3.0)));
        assert_eq!(store.put_at("p", window, keys(5000, 1, 1.0), now()), 1);
        assert_eq!(store.put_at("q", window, vec![("q.x".into(), Value::Number(1.0))], now()), 0);
        let sensors: Vec<_> = (0..1500).map(|i| (format!("hwinfo.s{i}"), Value::Number(1.0))).collect();
        assert_eq!(store.put_at("pc", window, sensors, now()), 0);
        assert_eq!(store.list(now()).len(), MOST_KEYS + 1 + 1500 + 1);
    }

    #[test]
    fn claude_values_last_an_hour() {
        let hour = Duration::from_secs(3600);
        assert_eq!(source("claude", None).stale_after(), hour);
        assert_eq!(source("claude", Some(60.0)).stale_after(), hour); // not 3 x interval
        assert_eq!(source("pc", Some(5.0)).stale_after(), Duration::from_secs(15));
        assert_eq!(source("homeassistant", None).stale_after(), Duration::from_secs(180));
        let store = Store::default();
        let claude = source("claude", None);
        store.put_at("claude", claude.stale_after(), vec![("claude.week".into(), Value::Number(94.0))], now());
        assert!(store.get("claude.week", now() + TimeDelta::minutes(59)).is_some());
        assert!(store.get("claude.week", now() + TimeDelta::minutes(61)).is_none());
        // Claude backs off on any error, the others only when told to wait
        let plain = sources::Failure::from("connection refused");
        let throttled = sources::Failure::from("HTTP 429 Too Many Requests");
        assert!(claude.backs_off(&plain) && claude.backs_off(&throttled));
        assert!(!source("homeassistant", None).backs_off(&plain));
        assert!(source("homeassistant", None).backs_off(&throttled));
        assert!(source("weather", None).backs_off(&sources::Failure::from("503 Service Unavailable")));
    }

    #[test]
    fn backoff_schedule() {
        let mut b = Backoff::default();
        assert!(!b.active());
        let minutes: Vec<u64> = (0..7).map(|_| b.next(None).as_secs() / 60).collect();
        assert_eq!(minutes, [2, 4, 8, 16, 30, 30, 30]);
        assert!(b.active());
        b.reset();
        assert!(!b.active());
        assert_eq!(b.next(None), Duration::from_secs(120));
        // Retry-After wins, within reason, and still counts as a failure
        assert_eq!(b.next(Some(Duration::from_secs(45))), Duration::from_secs(45));
        assert_eq!(b.next(Some(Duration::from_secs(90_000))), RETRY_AFTER_MAX);
        assert_eq!(b.next(None), Duration::from_secs(16 * 60));
        assert_eq!(brief(Duration::from_secs(45)), "45s");
        assert_eq!(brief(Duration::from_secs(120)), "2m");
        assert_eq!(brief(Duration::from_secs(5400)), "1h 30m");
    }

    #[test]
    fn first_poll_waits_for_fresh_values() {
        let every = Duration::from_secs(300);
        assert_eq!(first_poll_delay(None, every, now()), Duration::ZERO);
        assert_eq!(first_poll_delay(Some(now() - TimeDelta::seconds(100)), every, now()), Duration::from_secs(200));
        assert_eq!(first_poll_delay(Some(now() - TimeDelta::seconds(300)), every, now()), Duration::ZERO);
        assert_eq!(first_poll_delay(Some(now() - TimeDelta::hours(2)), every, now()), Duration::ZERO);
        assert_eq!(first_poll_delay(Some(now() + TimeDelta::hours(1)), every, now()), Duration::ZERO); // a clock gone backwards
    }

    #[test]
    fn last_values_round_trip() {
        let file = std::env::temp_dir().join(format!("wvplay-last-values-{}.json", std::process::id()));
        let store = Store::default();
        let claude = source("claude", None);
        let resets = Utc.with_ymd_and_hms(2026, 10, 14, 22, 0, 0).unwrap();
        let claude_at = now() - TimeDelta::minutes(50);
        store.put_at("claude", claude.stale_after(), vec![
            ("claude.week".into(), Value::Number(94.0)),
            ("claude.week_severity".into(), Value::Text("critical".into())),
            ("claude.week_resets".into(), Value::Time(resets)),
            ("claude.week_resets_in".into(), Value::Until(resets)),
        ], claude_at);
        store.put_at("pc", Duration::from_secs(15), vec![("pc.cpu.load".into(), Value::Number(12.5))], now() - TimeDelta::seconds(20));
        store.put_at("homeassistant", Duration::from_secs(180), vec![("ha.x".into(), Value::Number(1.0))], now());
        store.save(&file).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains("token") && !text.contains("time\":{"), "{text}");

        // only the sources still set up come back, with their real times and rules
        let again = Store::default();
        let loaded = again.load(&file, |kind| match kind {
            "claude" => Some(claude.stale_after()),
            "pc" => Some(Duration::from_secs(15)),
            _ => None,
        });
        std::fs::remove_file(&file).unwrap();
        assert_eq!(loaded, [("claude".to_string(), claude_at), ("pc".to_string(), now() - TimeDelta::seconds(20))]);
        assert_eq!(again.get("claude.week", now()), Some(Value::Number(94.0)));
        assert_eq!(again.get("claude.week_severity", now()), Some(Value::Text("critical".into())));
        assert_eq!(again.get("claude.week_resets", now()), Some(Value::Time(resets)));
        assert_eq!(again.get("claude.week_resets_in", now()), Some(Value::Until(resets)));
        assert_eq!(again.get("ha.x", now()), None);
        // the ages carried over: claude is fresh for 10 more minutes, pc's 20 s old value is already stale
        assert!(again.get("claude.week", now() + TimeDelta::minutes(9)).is_some());
        assert!(again.get("claude.week", now() + TimeDelta::minutes(11)).is_none());
        let listed: Vec<(String, DateTime<Utc>, bool)> = again.list(now()).into_iter()
            .filter(|(k, ..)| k != "time").map(|(k, _, at, stale, _)| (k, at, stale)).collect();
        assert_eq!(listed[0], ("claude.week".to_string(), claude_at, false));
        assert_eq!(listed[4], ("pc.cpu.load".to_string(), now() - TimeDelta::seconds(20), true));
        // a missing or broken file is nothing
        assert!(Store::default().load(&file, |_| Some(Duration::from_secs(1))).is_empty());
    }

    #[test]
    fn config() {
        let c: Config = serde_json::from_str(r#"{ "sources": [ { "type": "claude", "interval": 10 },
            { "type": "pc", "interval": 1, "hwinfo": true, "enabled": false }, { "type": "homeassistant" } ] }"#).unwrap();
        assert_eq!(c.api_port, DEFAULT_PORT);
        let every: Vec<u64> = c.sources.iter().map(|s| s.every().as_secs()).collect();
        assert_eq!(every, [60, 5, 60]);
        let claude: Source = serde_json::from_str(r#"{ "type": "claude" }"#).unwrap();
        assert_eq!(claude.every().as_secs(), 300);
        assert!(c.sources[0].enabled && !c.sources[1].enabled && c.sources[1].hwinfo);
        // a plugin's settings ride along, and the manifest's interval is the default
        let s: Source = serde_json::from_str(r#"{ "type": "weather", "latitude": 59.9, "units": "celsius" }"#).unwrap();
        assert_eq!(s.setting("latitude"), Some(json!(59.9)));
        assert_eq!(s.setting("units"), Some(json!("celsius")));
        assert_eq!(s.setting("token"), None);
        assert_eq!(s.every().as_secs(), 60);
        assert_eq!(c.sources[1].setting("hwinfo"), Some(json!(true)));
        // Libre Hardware Monitor is off unless set, at its default address unless set
        let pc = &c.sources[1];
        assert!(!pc.lhm && pc.lhm_url == sources::LHM_URL);
        assert_eq!(pc.setting("lhm"), Some(json!(false)));
        assert_eq!(pc.setting("lhm_url"), Some(json!("http://127.0.0.1:8085")));
        let pc: Source = serde_json::from_str(r#"{ "type": "pc", "lhm": true, "lhm_url": "http://192.168.1.5:8086" }"#).unwrap();
        assert!(pc.lhm && pc.extra.is_empty());
        assert_eq!(pc.setting("lhm_url"), Some(json!("http://192.168.1.5:8086")));
        assert_eq!((pc.setting("lhm_user"), pc.setting("lhm_password")), (None, None));
        let pc: Source = serde_json::from_str(r#"{ "type": "pc", "lhm_user": "cooler", "lhm_password": "pw" }"#).unwrap();
        assert!(pc.extra.is_empty() && pc.lhm_user == "cooler" && pc.lhm_password == "pw");
        assert_eq!((pc.setting("lhm_user"), pc.setting("lhm_password")), (Some(json!("cooler")), Some(json!("pw"))));
        assert_eq!(plugins_for(Path::new("C:/x/playlist/playlist.json")), Path::new("C:/x/plugins"));
    }

    /// A plugin's settings are its own, even under the keys the built-ins use: any type, an empty one
    /// kept as it is, and an unset one left to the manifest's default.
    #[test]
    fn plugin_settings_named_like_builtins() {
        let c: Config = serde_json::from_str(r#"{ "sources": [ { "type": "pc", "hwinfo": true },
            { "type": "myplugin", "url": "", "token": "abc", "hwinfo": "yes", "entities": "one", "lhm_url": 5, "interval": 30 } ] }"#).unwrap();
        let p = &c.sources[1];
        assert_eq!(p.setting("url"), Some(json!("")));
        assert_eq!(p.setting("token"), Some(json!("abc")));
        assert_eq!(p.setting("hwinfo"), Some(json!("yes")));
        assert_eq!(p.setting("entities"), Some(json!("one")));
        assert_eq!(p.setting("lhm_url"), Some(json!(5)));
        assert_eq!(p.setting("lhm"), None);
        assert_eq!((p.enabled, p.interval), (true, Some(30.0)));
        assert!(c.sources[0].hwinfo && c.sources[0].extra.is_empty());
        // a built-in's are still typed, and still read the way they were
        assert!(serde_json::from_str::<Source>(r#"{ "type": "pc", "hwinfo": "yes" }"#).is_err());
        let ha: Source = serde_json::from_str(r#"{ "type": "homeassistant", "url": "", "token": "t", "entities": ["sensor.x"] }"#).unwrap();
        assert_eq!((ha.setting("url"), ha.token.as_str(), ha.setting("entities")), (None, "t", Some(json!(["sensor.x"]))));
        // and a saved password stands in for a plugin's token like any other
        let mut test: Source = serde_json::from_str(r#"{ "type": "myplugin", "url": "" }"#).unwrap();
        assert!(test.lacks_password("token"));
        test.copy_setting("token", p);
        assert_eq!(test.setting("token"), Some(json!("abc")));
    }

    /// A poll that's told to wait backs off; a stream isn't polled, so it doesn't, and says nothing of
    /// a wait.
    #[test]
    fn streams_never_back_off() {
        let hub = Hub { file: PathBuf::new(), config: Mutex::default(), running: Mutex::default(), status: Mutex::default(),
                        server: Mutex::default(), plugins: PathBuf::new(), saved_at: Mutex::default() };
        let s = source("throttled", Some(60.0));
        let throttled = || Err(sources::Failure::from("HTTP 429 Too Many Requests"));
        let (mut failing, mut backoff) = (None, Backoff::default());
        assert_eq!(hub.record(&s, throttled(), &mut failing, &mut backoff, false), BACKOFF_FIRST);
        assert!(backoff.active());
        let (mut failing, mut backoff) = (None, Backoff::default());
        assert_eq!(hub.record(&s, throttled(), &mut failing, &mut backoff, true), s.every());
        assert!(!backoff.active());
        assert_eq!(failing.as_deref(), Some("HTTP 429 Too Many Requests"));
        assert_eq!(hub.status.lock().unwrap()["throttled"].result, Err("HTTP 429 Too Many Requests".to_string()));
    }

    #[test]
    fn builtin_settings_are_fields() {
        // every setting of a built-in source is read into its own field, so setting(), copy_setting() and
        // the poll all see it; none is left over as if it were a plugin's
        for m in catalog::builtins() {
            for s in &m.settings {
                let value = match s.kind.as_str() {
                    "bool" => json!(true),
                    "list" => json!(["x"]),
                    _ => json!("x"),
                };
                let mut set = json!({ "type": m.id });
                set[s.key.as_str()] = value.clone();
                let source: Source = serde_json::from_value(set).unwrap();
                assert!(source.extra.is_empty(), "{}.{}", m.id, s.key);
                assert_eq!(source.setting(&s.key), Some(value), "{}.{}", m.id, s.key);
                if s.kind == "password" {
                    let mut blank: Source = serde_json::from_value(json!({ "type": m.id })).unwrap();
                    assert!(blank.lacks_password(&s.key));
                    blank.copy_setting(&s.key, &source);
                    assert!(!blank.lacks_password(&s.key), "{}.{}", m.id, s.key);
                }
            }
        }
    }

    /// A hub with this data.json, polling nothing: for /test.
    fn hub_with(sources: Vec<Source>) -> Hub {
        Hub { file: PathBuf::new(), config: Mutex::new(Config { sources, ..Config::default() }), running: Mutex::default(),
              status: Mutex::default(), server: Mutex::default(), plugins: PathBuf::new(), saved_at: Mutex::default() }
    }

    #[test]
    fn lhm_password_stays_with_its_address() {
        // two LHMs with the same login: A, where the password was saved, and B, an address typed since
        let (user, password) = ("cooler", "s3cret-lhm");
        let (a, at_a) = sources::fake::lhm(user, password);
        let (b, at_b) = sources::fake::lhm(user, password);
        let saved: Source = serde_json::from_value(json!({ "type": "pc", "lhm": true, "lhm_url": a, "lhm_user": user,
                                                           "lhm_password": password })).unwrap();
        let hub = hub_with(vec![saved]);
        // what /test gets from the studio: no password, as the browser never has it
        let test = |url: &str, user: &str| {
            let source = json!({ "type": "pc", "enabled": true, "lhm": true, "lhm_url": url, "lhm_user": user });
            hub.test(serde_json::from_value(source).unwrap()).unwrap()
        };
        let auth = |requests: &std::sync::mpsc::Receiver<sources::fake::Seen>| {
            requests.recv_timeout(Duration::from_secs(5)).expect("no request").1.unwrap_or_default()
        };
        let lhm_keys = |polled: &Polled| polled.values.iter().filter(|(k, _)| k.starts_with("lhm.")).count();
        let sent_password = |header: &str| {
            let login = sources::fake::unbase64(header.trim_start_matches("Basic ")).unwrap();
            String::from_utf8_lossy(&login).contains(password)
        };

        // its own address (written as the studio may): the saved password stands in
        for url in [a.clone(), format!("{a}/"), format!(" {a} ")] {
            let polled = test(&url, user);
            assert_eq!(lhm_keys(&polled), 35, "{url}: {:?}", polled.notes);
            assert!(sent_password(&auth(&at_a)));
        }

        // another address: it doesn't, so B never sees it, and says it refused what it got
        let polled = test(&b, user);
        let header = auth(&at_b);
        assert!(!sent_password(&header) && header.starts_with("Basic "), "{header}");
        assert_eq!(lhm_keys(&polled), 0);
        assert!(polled.notes.iter().any(|n| n.contains(&format!("at {b} refused the user name or password"))), "{:?}", polled.notes);
        assert!(polled.notes.iter().all(|n| !n.contains(password)));
        assert!(at_a.try_recv().is_err());

        // nor at A with another user name: the password has to be typed again
        let polled = test(&a, "someone");
        assert!(!sent_password(&auth(&at_a)));
        assert_eq!(lhm_keys(&polled), 0);

        // a password typed in the form is used, wherever it points
        let typed = json!({ "type": "pc", "lhm": true, "lhm_url": b, "lhm_user": user, "lhm_password": password });
        let polled = hub.test(serde_json::from_value(typed).unwrap()).unwrap();
        assert!(sent_password(&auth(&at_b)));
        assert_eq!(lhm_keys(&polled), 35);
        assert!(at_a.try_recv().is_err());
    }
}
