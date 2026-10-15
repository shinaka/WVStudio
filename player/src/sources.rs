//! Where live values come from (docs/data.md): Claude plan usage, the PC's own sensors (Windows, NVIDIA,
//! HWiNFO, Libre Hardware Monitor) and Home Assistant. Each poll returns its values, or what went wrong;
//! tokens never appear in either.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread::sleep;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value as Json;

use crate::data::{Source, Value};

/// A poll's values, plus notes on parts that were skipped (no NVIDIA driver, HWiNFO not running...).
#[derive(Debug)]
pub struct Polled {
    pub values: Vec<(String, Value)>,
    pub notes: Vec<String>,
}

/// What a source keeps between polls.
#[derive(Default)]
pub struct State {
    /// The last GetSystemTimes sample (idle, kernel, user), for the CPU load since.
    cpu: Option<[u64; 3]>,
}

/// Why a poll failed: the message (never a token or a header), the HTTP status when there was one, and
/// how long the server asked us to wait (`Retry-After`), so the Hub can back off.
#[derive(Debug, PartialEq)]
pub struct Failure {
    pub message: String,
    pub status: Option<u16>,
    pub retry_after: Option<Duration>,
}

impl Failure {
    /// Whether the server asked for a pause: 429 (too many requests) or 503 (unavailable).
    pub fn throttled(&self) -> bool {
        matches!(self.status, Some(429 | 503))
    }
}

/// A plain message, with a 429 or 503 picked out of it when a plugin's error names one.
impl From<String> for Failure {
    fn from(message: String) -> Failure {
        let status = message
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|word| word.parse::<u16>().ok())
            .find(|code| matches!(code, 429 | 503));
        Failure { message, status, retry_after: None }
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Failure {
        Failure::from(message.to_string())
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub fn poll(source: &Source, state: &mut State) -> Result<Polled, Failure> {
    match source.kind.as_str() {
        "claude" => Ok(Polled { values: claude()?, notes: Vec::new() }),
        "pc" => Ok(pc(source, state)),
        "homeassistant" => homeassistant(source),
        "time" => Ok(Polled { values: Vec::new(), notes: Vec::new() }), // always there, never polled
        other => match crate::catalog::manifest(other) {
            Some(manifest) => crate::plugin::poll(source, &manifest).map_err(Failure::from),
            None => Err(format!("unknown source type {other:?}").into()),
        },
    }
}

/// How a GET is made.
struct Fetch {
    /// Given up after this, in all.
    timeout: Duration,
    /// The longest reply read, in bytes.
    limit: u64,
    /// Straight to the host, never through a proxy the environment names (HTTP_PROXY, ALL_PROXY...).
    direct: bool,
    /// To the host's IPv4 addresses only.
    ipv4: bool,
}

/// A web source's GET.
const WEB: Fetch = Fetch { timeout: Duration::from_secs(20), limit: 64 << 20, direct: false, ipv4: false };

/// A GET, as text. Errors say what failed (the status, say), never the headers.
fn get(url: &str, headers: &[(&str, &str)], fetch: &Fetch) -> Result<String, Failure> {
    let mut config = ureq::Agent::config_builder().timeout_global(Some(fetch.timeout)).http_status_as_error(false);
    if fetch.direct {
        config = config.proxy(None);
    }
    if fetch.ipv4 {
        config = config.ip_family(ureq::config::IpFamily::Ipv4Only);
    }
    let agent: ureq::Agent = config.build().into();
    let mut request = agent.get(url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut reply = request.call().map_err(|e| Failure::from(e.to_string()))?;
    let status = reply.status();
    if !status.is_success() {
        let retry_after = reply.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| retry_after(s, Utc::now()));
        let reason = status.canonical_reason().map(|r| format!(" {r}")).unwrap_or_default();
        return Err(Failure { message: format!("HTTP {}{reason}", status.as_u16()), status: Some(status.as_u16()), retry_after });
    }
    reply.body_mut().with_config().limit(fetch.limit).read_to_string().map_err(|e| Failure::from(e.to_string()))
}

/// A `Retry-After` header: seconds, or an HTTP date (RFC 2822), as how long to wait from `now`.
pub fn retry_after(header: &str, now: DateTime<Utc>) -> Option<Duration> {
    let header = header.trim();
    if let Ok(secs) = header.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = DateTime::parse_from_rfc2822(header).ok()?.with_timezone(&Utc);
    Some((at - now).to_std().unwrap_or(Duration::ZERO))
}

const USAGE: &str = "https://api.anthropic.com/api/oauth/usage";

/// Claude plan usage, from the endpoint Claude Code's /usage uses, with Claude Code's own login. The
/// login is only read: Claude Code renews it whenever it runs.
fn claude() -> Result<Vec<(String, Value)>, Failure> {
    let home = std::env::var_os("USERPROFILE").ok_or("USERPROFILE isn't set")?;
    let file = PathBuf::from(home).join(".claude").join(".credentials.json");
    let login: Json = std::fs::read(&file)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or("no Claude Code login to read (sign in to Claude Code)")?;
    let oauth = &login["claudeAiOauth"];
    let token = oauth["accessToken"].as_str().filter(|t| !t.is_empty()).ok_or("the Claude Code login has no token")?;
    if oauth["expiresAt"].as_f64().is_some_and(|ms| ms <= Utc::now().timestamp_millis() as f64) {
        return Err("the Claude Code login has expired (Claude Code renews it when it runs)".into());
    }
    let reply = get(USAGE, &[("Authorization", &format!("Bearer {token}")), ("anthropic-beta", "oauth-2025-04-20"),
                             ("User-Agent", "wvplay")], &WEB)?;
    let usage: Json = serde_json::from_str(&reply).map_err(|_| Failure::from("the usage reply isn't JSON"))?;
    Ok(claude_values(&usage))
}

/// The endpoint is undocumented, so anything missing is just left out.
fn claude_values(usage: &Json) -> Vec<(String, Value)> {
    let mut values = Vec::new();
    for (field, name) in [("five_hour", "session"), ("seven_day", "week")] {
        let window = &usage[field];
        if let Some(percent) = window["utilization"].as_f64() {
            values.push((format!("claude.{name}"), Value::Number(percent)));
        }
        if let Some(at) = window["resets_at"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()) {
            let at = at.with_timezone(&Utc);
            values.push((format!("claude.{name}_resets"), Value::Time(at)));
            values.push((format!("claude.{name}_resets_in"), Value::Until(at)));
        }
    }
    for limit in usage["limits"].as_array().into_iter().flatten() {
        let severity = limit["severity"].as_str().map(|s| Value::Text(s.into()));
        match limit["kind"].as_str() {
            Some("session") => values.extend(severity.map(|s| ("claude.session_severity".into(), s))),
            Some("weekly_all") => values.extend(severity.map(|s| ("claude.week_severity".into(), s))),
            Some("weekly_scoped") => {
                let name = limit["scope"]["model"]["display_name"].as_str().filter(|n| !n.is_empty());
                if let (Some(name), Some(percent)) = (name, limit["percent"].as_f64()) {
                    values.push((format!("claude.week.{}", name.to_lowercase()), Value::Number(percent)));
                }
            }
            _ => {}
        }
    }
    values
}

fn homeassistant(source: &Source) -> Result<Polled, Failure> {
    let url = source.url.trim().trim_end_matches('/');
    if url.is_empty() {
        return Err("no URL set".into());
    }
    if source.token.is_empty() {
        return Err("no token set".into());
    }
    let reply = get(&format!("{url}/api/states"), &[("Authorization", &format!("Bearer {}", source.token))], &WEB)?;
    let states: Json = serde_json::from_str(&reply).map_err(|_| Failure::from("the states reply isn't JSON"))?;
    Ok(ha_values(&states, &source.entities))
}

/// The listed entities' states, units and plain attributes, from the reply to /api/states.
fn ha_values(states: &Json, entities: &[String]) -> Polled {
    let wanted: Vec<&str> = entities.iter().map(|e| e.trim()).filter(|e| !e.is_empty()).collect();
    let (mut values, mut found) = (Vec::new(), Vec::new());
    for state in states.as_array().into_iter().flatten() {
        let Some(id) = state["entity_id"].as_str().filter(|id| wanted.contains(id)) else { continue };
        found.push(id);
        let key = format!("ha.{id}");
        if let Some(s) = state["state"].as_str() {
            values.push((key.clone(), Value::guess(s)));
        }
        for (name, attribute) in state["attributes"].as_object().into_iter().flatten() {
            let value = match attribute {
                Json::Number(n) => n.as_f64().map(Value::Number),
                Json::String(s) => Some(Value::Text(s.clone())),
                Json::Bool(b) => Some(Value::Text(b.to_string())),
                _ => None, // lists, objects and nulls aren't plain values
            };
            let Some(value) = value else { continue };
            if name == "unit_of_measurement" {
                values.push((format!("{key}.unit"), value.clone()));
            }
            values.push((format!("{key}.{}", name.to_lowercase()), value));
        }
    }
    let notes = wanted.iter().filter(|w| !found.contains(w)).map(|w| format!("no entity {w}")).collect();
    Polled { values, notes }
}

/// The PC's own sensors: CPU load and memory from Windows, the GPU from NVIDIA's driver, and (with
/// `hwinfo`, `lhm`) everything HWiNFO or Libre Hardware Monitor reads. A part that can't be read is a
/// note, never a failure, so the rest still comes through.
fn pc(source: &Source, state: &mut State) -> Polled {
    let (mut values, mut notes) = (Vec::new(), Vec::new());
    let mut put = |key: &str, value: f64| values.push((key.to_string(), Value::Number(value)));

    // CPU load since the last poll; the first time, over half a second
    let before = state.cpu.or_else(|| {
        let first = win::cpu_times();
        sleep(Duration::from_millis(500));
        first
    });
    let now = win::cpu_times();
    if let (Some([idle0, kernel0, user0]), Some([idle1, kernel1, user1])) = (before, now) {
        // kernel time includes idle time
        let total = (kernel1 - kernel0) + (user1 - user0);
        if total > 0 {
            put("pc.cpu.load", 100.0 * total.saturating_sub(idle1 - idle0) as f64 / total as f64);
        }
    }
    state.cpu = now;

    if let Some((total, available)) = win::memory() {
        let gb = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
        put("pc.ram.used_gb", gb(total - available));
        put("pc.ram.total_gb", gb(total));
        put("pc.ram.used_pct", 100.0 * (total - available) as f64 / total.max(1) as f64);
    }

    match win::nvml() {
        Ok(gpu) => {
            let gb = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
            let gpu = gpu.read();
            gpu.temp.map(|t| put("pc.gpu.temp", t as f64));
            gpu.load.map(|l| put("pc.gpu.load", l as f64));
            gpu.power_mw.map(|p| put("pc.gpu.power", p as f64 / 1000.0));
            if let Some((total, used)) = gpu.memory {
                put("pc.gpu.vram_used_gb", gb(used));
                put("pc.gpu.vram_total_gb", gb(total));
            }
        }
        Err(e) => notes.push(e.to_string()),
    }

    if source.hwinfo {
        match win::hwinfo().and_then(|bytes| hwinfo_readings(&bytes)) {
            Ok(readings) => values.extend(hwinfo_values(&readings)),
            Err(e) => notes.push(e),
        }
    }
    if source.lhm {
        match lhm(&source.lhm_url) {
            Ok((found, note)) => {
                add_lhm(&mut values, found);
                notes.extend(note);
            }
            Err(e) => notes.push(e),
        }
    }
    Polled { values, notes }
}

/// Where Libre Hardware Monitor's web server answers unless set otherwise.
pub const LHM_URL: &str = "http://127.0.0.1:8085";

/// How long the pc poll waits for Libre Hardware Monitor, so one that isn't there never stalls it.
const LHM_TIMEOUT: Duration = Duration::from_secs(3);

/// The most of LHM's reply the player reads, and the most sensors it takes from it. A real data.json is
/// well under 1 MB, with a few hundred sensors: this keeps anything else that answers at the address from
/// filling the store with keys, which it keeps.
const LHM_LIMIT: u64 = 4 << 20;
const LHM_MAX_SENSORS: usize = 4096;

/// Libre Hardware Monitor's sensors, from its web server's data.json, and a note when some were left out.
/// An error is the pc poll's note.
fn lhm(url: &str) -> Result<(Vec<(String, Value)>, Option<String>), String> {
    let base = lhm_base(url);
    let not_answering = |why: &str| {
        format!("Libre Hardware Monitor isn't answering at {base} ({why}): check that it's running, with Options > Remote Web Server on")
    };
    let reply = get(&format!("{base}/data.json"), &[], &lhm_fetch(&base)).map_err(|e| not_answering(&lhm_why(&e)))?;
    let tree = serde_json::from_str::<Json>(&reply).ok().filter(|tree| tree["Children"].is_array());
    let tree = tree.ok_or_else(|| not_answering("its reply isn't sensor data"))?;
    let mut sensors = lhm_sensors(&tree);
    let note = (sensors.len() > LHM_MAX_SENSORS).then(|| {
        format!("Libre Hardware Monitor at {base} lists {} sensors: only the first {LHM_MAX_SENSORS} are read", sensors.len())
    });
    sensors.truncate(LHM_MAX_SENSORS);
    Ok((lhm_values(&sensors), note))
}

/// How LHM is asked: within LHM_TIMEOUT, for LHM_LIMIT at most. On this PC, never through a proxy, which
/// HTTP_PROXY or ALL_PROXY would otherwise send even 127.0.0.1 to. `localhost` is asked at 127.0.0.1 only:
/// LHM answers there whenever it answers at ::1, and Windows takes about 2 s to give up on each address,
/// so with nothing listening, trying ::1 first would use up the time and hide why.
fn lhm_fetch(base: &str) -> Fetch {
    let host = base.parse::<ureq::http::Uri>().ok().and_then(|uri| uri.host().map(str::to_ascii_lowercase)).unwrap_or_default();
    let localhost = host == "localhost";
    let loopback = localhost || host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    Fetch { timeout: LHM_TIMEOUT, limit: LHM_LIMIT, direct: loopback, ipv4: localhost }
}

/// Why a GET failed, briefly.
fn lhm_why(e: &Failure) -> String {
    let lower = e.message.to_lowercase();
    if e.status == Some(401) {
        "it asks for a password (Remote Web Server > Authentication), which the player doesn't send".into()
    } else if e.status.is_some() {
        e.message.clone()
    } else if lower.contains("larger than") {
        format!("its reply is over {} MB, far more than LHM's", LHM_LIMIT >> 20)
    } else if lower.contains("timeout") || lower.contains("timed out") {
        format!("no reply in {} s", LHM_TIMEOUT.as_secs())
    } else if lower.contains("refused") || lower.contains("10061") {
        "nothing is listening there".into()
    } else {
        e.message.clone()
    }
}

/// The address as set: the default when blank, `http://` when it names no scheme, without a trailing `/`
/// or `/data.json` (the address a browser shows for the data, which LHM's server would take as a file).
fn lhm_base(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    // ASCII lowercasing keeps every byte where it was, so the end that matched is the same length
    let url = if url.to_ascii_lowercase().ends_with("/data.json") { url[..url.len() - 10].trim_end_matches('/') } else { url };
    match url {
        "" => LHM_URL.to_string(),
        url if url.contains("://") => url.to_string(),
        url => format!("http://{url}"),
    }
}

/// Adds Libre Hardware Monitor's values to the pc poll's. HWiNFO's `pc.cpu.temp`, when it gave one, wins.
fn add_lhm(values: &mut Vec<(String, Value)>, lhm: Vec<(String, Value)>) {
    let has_temp = values.iter().any(|(k, _)| k == "pc.cpu.temp");
    values.extend(lhm.into_iter().filter(|(k, _)| !(has_temp && k == "pc.cpu.temp")));
}

/// One Libre Hardware Monitor sensor: its hardware's key (numbered when two have the same name), whether
/// that hardware is a CPU, its type (`temperature`, `load`...), its name, its name as a key part, and its
/// value (None when it has no reading: "-").
struct LhmSensor {
    hardware: String,
    cpu: bool,
    kind: String,
    name: String,
    key: String,
    value: Option<f64>,
}

/// The hardware the walk is in.
#[derive(Default)]
struct LhmHardware {
    key: String,
    /// From its `HardwareId` (`/amdcpu/0`), which only newer versions give.
    cpu: Option<bool>,
    /// Whether it has the CPU icon, for versions with no ids at all.
    cpu_icon: bool,
}

/// Every sensor in data.json, in tree order. The tree is a root ("Sensor"), the computer, its hardware
/// (which can hold more, like a motherboard's SuperIO chip), a group per sensor type ("Temperatures"),
/// then the sensors. Sensors have `SensorId` and `Type` (0.8.7 on), and since 0.9.5 `RawValue`, while
/// hardware has `HardwareId`. Without those (Open Hardware Monitor's shape), a group is told by its
/// children all being leaves.
fn lhm_sensors(tree: &Json) -> Vec<LhmSensor> {
    let mut sensors = Vec::new();
    lhm_walk(tree, &LhmHardware::default(), None, &mut HashMap::new(), &mut sensors);
    sensors
}

fn lhm_walk(node: &Json, hardware: &LhmHardware, group: Option<&str>, named: &mut HashMap<String, usize>,
            out: &mut Vec<LhmSensor>) {
    let text = node["Text"].as_str().unwrap_or_default();
    let children = node["Children"].as_array().map(Vec::as_slice).unwrap_or_default();
    let leaf = |n: &Json| n["Children"].as_array().is_none_or(Vec::is_empty);
    let sensor = |n: &Json| n.get("SensorId").is_some() || n.get("Type").is_some();
    if sensor(node) || (group.is_some() && leaf(node)) {
        // kept even with no reading, so that the keys after it stay where they are
        let kind = match node["Type"].as_str() {
            Some(kind) => slug(kind),
            None => lhm_type(group.unwrap_or_default()),
        };
        let id = node["SensorId"].as_str();
        let cpu = hardware.cpu.or_else(|| id.map(lhm_cpu_id)).unwrap_or(hardware.cpu_icon);
        // a sensor's id ends in its type and number: `/amdcpu/0/temperature/2` is `temperature_2`
        let id_end = id.map(|id| id.rsplitn(3, '/').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("_"));
        let key = lhm_name(text, id_end.as_deref(), "sensor");
        out.push(LhmSensor { hardware: hardware.key.clone(), cpu, kind, name: text.to_string(), key, value: lhm_value(node) });
    } else if node.get("HardwareId").is_none() && !children.is_empty() && children.iter().all(|c| leaf(c) || sensor(c)) {
        for child in children {
            lhm_walk(child, hardware, Some(text), named, out);
        }
    } else {
        // hardware, or the root and the computer above it
        let mut key = lhm_name(text, node["HardwareId"].as_str(), "hardware");
        let count = named.entry(key.clone()).or_default();
        *count += 1;
        if *count > 1 {
            key = format!("{key}_{count}");
        }
        let here = LhmHardware {
            key,
            cpu: node["HardwareId"].as_str().map(lhm_cpu_id),
            cpu_icon: node["ImageURL"].as_str().is_some_and(|u| u.ends_with("/cpu.png")),
        };
        for child in children {
            lhm_walk(child, &here, None, named, out);
        }
    }
}

/// Whether an LHM id (`/amdcpu/0`, `/intelcpu/0/temperature/1`) is a CPU's.
fn lhm_cpu_id(id: &str) -> bool {
    id.trim_start_matches('/').split('/').next().is_some_and(|first| first.to_ascii_lowercase().contains("cpu"))
}

/// A sensor's type from its group's name, for versions that don't give `Type`: "Temperatures" is
/// `temperature` (LHM's TypeNode names).
fn lhm_type(group: &str) -> String {
    const GROUPS: [(&str, &str); 20] = [
        ("voltages", "voltage"), ("currents", "current"), ("capacities", "energy"), ("clocks", "clock"),
        ("load", "load"), ("temperatures", "temperature"), ("fans", "fan"), ("flows", "flow"),
        ("controls", "control"), ("levels", "level"), ("powers", "power"), ("data", "data"),
        ("factors", "factor"), ("frequencies", "frequency"), ("throughput", "throughput"), ("times", "timespan"),
        ("timings", "timing"), ("noise_levels", "noise"), ("conductivities", "conductivity"),
        ("humidity_levels", "humidity"),
    ];
    let group = slug(group);
    match GROUPS.iter().find(|(name, _)| *name == group) {
        Some((_, kind)) => kind.to_string(),
        None => group.strip_suffix('s').unwrap_or(&group).to_string(), // one LHM added since: its name, singular
    }
}

/// A sensor's value: `RawValue` when there is one (a number in the newest versions, text like "45.3 °C" in
/// 0.9.5 and 0.9.6), else what `Value` shows. None when it has none ("-").
fn lhm_value(node: &Json) -> Option<f64> {
    let raw = match &node["RawValue"] {
        // LHM's newest code gives data in bytes, where 0.9.6 gave GB: GB, as before
        Json::Number(n) if node["Type"] == "Data" => n.as_f64().map(|bytes| bytes / (1u64 << 30) as f64),
        Json::Number(n) => n.as_f64(),
        Json::String(s) => lhm_reading(s),
        _ => None,
    };
    raw.or_else(|| node["Value"].as_str().and_then(lhm_reading)).filter(|v| v.is_finite())
}

/// A reading as LHM writes it, in the PC's Regional format: "45.3 °C", "45,3 °C", "45٫3 °C", "‎−5.0 °C",
/// "1.234568e+07" (a big time span in seconds, on .NET Framework), "1:02:03" (a time span, as seconds). A
/// temperature shown in °F is turned back into °C, and throughput shown scaled ("1.2 MB/s", "100Mbps", what
/// `Value` shows) back into B/s (bit/s for "Connection Speed"). None when it doesn't start with a number.
fn lhm_reading(s: &str) -> Option<f64> {
    let s = s.trim_matches(|c: char| c.is_whitespace() || bidi_mark(c));
    match s {
        // the two link speeds LHM shows as they are
        "100Mbps" => return Some(1e8),
        "1Gbps" => return Some(1e9),
        _ => {}
    }
    if s.contains(':') {
        // [d:]h:mm:ss[.fff]
        let parts: Vec<f64> = s.split(':').map(|part| leading_number(part).map(|(n, _)| n)).collect::<Option<_>>()?;
        let weights = [86400.0, 3600.0, 60.0, 1.0];
        return (parts.len() <= weights.len()).then(|| parts.iter().rev().zip(weights.iter().rev()).map(|(p, w)| p * w).sum());
    }
    let (n, unit) = leading_number(s)?;
    let scale = match unit.trim() {
        "°F" => return Some((n - 32.0) / 1.8),
        "KB/s" | "Kbps" => 1024.0,
        "MB/s" | "Mbps" => 1024.0 * 1024.0,
        "GB/s" | "Gbps" => 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    Some(n * scale)
}

/// The number a text starts with, and the rest of the text. The decimal point is `.`, `,`, `٫` (Arabic), or
/// `/` between digits (Persian, on .NET Framework). The sign is `-` or `−`, with any bidi marks around it,
/// and an exponent (`e+07`) is applied.
fn leading_number(s: &str) -> Option<(f64, &str)> {
    fn sign(s: &str) -> (bool, &str) {
        let s = s.trim_start_matches(bidi_mark);
        match s.strip_prefix(['-', '−']) {
            Some(rest) => (true, rest.trim_start_matches(bidi_mark)),
            None => (false, s.strip_prefix('+').unwrap_or(s).trim_start_matches(bidi_mark)),
        }
    }
    fn digits(s: &str) -> (&str, &str) {
        s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()))
    }
    let (negative, s) = sign(s.trim_start());
    let (whole, mut rest) = digits(s);
    if whole.is_empty() {
        return None;
    }
    let mut number = whole.to_string();
    if let Some(point) = rest.chars().next().filter(|c| matches!(c, '.' | ',' | '٫' | '/')) {
        let (fraction, after) = digits(&rest[point.len_utf8()..]);
        if point != '/' || !fraction.is_empty() {
            number = format!("{whole}.{fraction}");
            rest = after;
        }
    }
    if let Some(after) = rest.strip_prefix(['e', 'E']) {
        let (exponent_negative, after) = sign(after);
        let (exponent, after) = digits(after);
        if !exponent.is_empty() {
            number = format!("{number}e{}{exponent}", if exponent_negative { "-" } else { "" });
            rest = after;
        }
    }
    let n: f64 = number.parse().ok()?;
    Some((if negative { -n } else { n }, rest))
}

/// The invisible marks some Regional formats put around a number's sign (U+200E before "-5.5" in Hebrew,
/// U+061C in Arabic), and their kin.
fn bidi_mark(c: char) -> bool {
    matches!(c, '\u{61c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
}

/// A hardware or sensor name as a key part: slugified, with accents dropped ("Température" is
/// `temperature`). When that leaves nothing (a name in another script, like a NIC that Windows calls
/// "イーサネット"), it's the slugified id (`nic_<guid>`, `temperature_3`), else `fallback`.
fn lhm_name(name: &str, id: Option<&str>, fallback: &str) -> String {
    [slug(&plain_latin(name)), id.map(slug).unwrap_or_default()]
        .into_iter()
        .find(|key| !key.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// Lowercase, with Latin letters' accents dropped: "Conexión" is "conexion", "Lüfter" "lufter".
fn plain_latin(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        out.push_str(match c {
            'à'..='å' | 'ā' | 'ă' | 'ą' => "a",
            'æ' => "ae",
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
            'ð' | 'ď' | 'đ' => "d",
            'è'..='ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
            'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
            'ĥ' | 'ħ' => "h",
            'ì'..='ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
            'ĳ' => "ij",
            'ĵ' => "j",
            'ķ' => "k",
            'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
            'ñ' | 'ń' | 'ņ' | 'ň' => "n",
            'ò'..='ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
            'œ' => "oe",
            'ŕ' | 'ŗ' | 'ř' => "r",
            'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => "s",
            'ß' => "ss",
            'ţ' | 'ť' | 'ŧ' | 'ț' => "t",
            'þ' => "th",
            'ù'..='ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
            'ŵ' => "w",
            'ý' | 'ÿ' | 'ŷ' => "y",
            'ź' | 'ż' | 'ž' => "z",
            '\u{300}'..='\u{36f}' => "", // an accent written apart from its letter
            c => {
                out.push(c);
                continue;
            }
        });
    }
    out
}

/// Every sensor with a reading as `lhm.<hardware>.<type>.<sensor>`, plus `pc.cpu.temp`: the CPU die
/// temperature. A key that's taken gets `_2`, `_3`... in tree order, counting the sensors with no reading
/// too, so a sensor's key doesn't change when another one's reading comes and goes.
fn lhm_values(sensors: &[LhmSensor]) -> Vec<(String, Value)> {
    let mut values = Vec::new();
    let mut taken = HashSet::new();
    for s in sensors {
        let kind = if s.kind.is_empty() { "other" } else { &s.kind };
        let base = format!("lhm.{}.{kind}.{}", s.hardware, s.key);
        let (mut key, mut n) = (base.clone(), 1);
        while !taken.insert(key.clone()) {
            n += 1;
            key = format!("{base}_{n}");
        }
        if let Some(value) = s.value {
            values.push((key, Value::Number(value)));
        }
    }
    if let Some(temp) = lhm_cpu_temp(sensors) {
        values.push(("pc.cpu.temp".into(), Value::Number(temp)));
    }
    values
}

/// The CPU die temperature: "Core (Tctl/Tdie)" on AMD ("Core (Tdie)", then "Core (Tctl)", when LHM shows
/// them apart), "CPU Package" on Intel, else "Core Max", else "Core Average", else the CPU's first. Only
/// sensors with a reading count.
fn lhm_cpu_temp(sensors: &[LhmSensor]) -> Option<f64> {
    let rank = |s: &&LhmSensor| match s.name.to_lowercase().as_str() {
        "core (tctl/tdie)" | "cpu package" => 0,
        "core (tdie)" => 1,
        "core (tctl)" => 2,
        "core max" => 3,
        "core average" => 4,
        _ => 5,
    };
    sensors.iter().filter(|s| s.cpu && s.kind == "temperature" && s.value.is_some()).min_by_key(rank).and_then(|s| s.value)
}

/// One HWiNFO reading: its sensor's name, its label, its type (1 is a temperature) and its value.
struct Reading {
    sensor: String,
    label: String,
    kind: u32,
    value: f64,
}

const TEMPERATURE: u32 = 1;

/// Reads HWiNFO's shared memory (the layout in its SDK's hwisenssm2.h: a header, then fixed-size sensor
/// and reading records at the offsets it gives).
fn hwinfo_readings(bytes: &[u8]) -> Result<Vec<Reading>, String> {
    let u32_at = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
    let text = |at: usize| {
        let raw = bytes.get(at..at + 128).unwrap_or_default();
        String::from_utf8_lossy(&raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())]).into_owned()
    };
    if u32_at(0) != Some(0x5369_5748) {
        return Err("HWiNFO's shared memory isn't active (is Shared Memory Support on?)".into());
    }
    let field = |at| u32_at(at).ok_or("HWiNFO's shared memory is cut short");
    let (sensors_at, sensor_size, sensor_count) = (field(20)?, field(24)?, field(28)?);
    let (readings_at, reading_size, reading_count) = (field(32)?, field(36)?, field(40)?);
    // a sensor: id, instance, original name, the user's name for it
    let named = |user: usize, original: usize| Some(text(user)).filter(|t| !t.is_empty()).unwrap_or_else(|| text(original));
    let sensors: Vec<String> = (0..sensor_count).map(|i| sensors_at + i * sensor_size).map(|at| named(at + 136, at + 8)).collect();
    let mut readings = Vec::with_capacity(reading_count);
    for i in 0..reading_count {
        // a reading: type, sensor index, id, original label, the user's label, unit, value, ...
        let at = readings_at + i * reading_size;
        let (Some(kind), Some(sensor)) = (u32_at(at), u32_at(at + 4)) else { break };
        let Some(value) = bytes.get(at + 284..at + 292).map(|b| f64::from_le_bytes(b.try_into().unwrap())) else { break };
        let sensor = sensors.get(sensor).cloned().unwrap_or_default();
        readings.push(Reading { sensor, label: named(at + 140, at + 12), kind: kind as u32, value });
    }
    Ok(readings)
}

/// Every reading as `hwinfo.<sensor>.<label>`, plus `pc.cpu.temp`: the CPU die temperature ("CPU
/// (Tctl/Tdie)" on AMD), else the first CPU temperature.
fn hwinfo_values(readings: &[Reading]) -> Vec<(String, Value)> {
    let mut values: Vec<(String, Value)> = Vec::new();
    for r in readings.iter().filter(|r| r.value.is_finite()) {
        let key = format!("hwinfo.{}.{}", slug(&r.sensor), slug(&r.label));
        if !values.iter().any(|(k, _)| *k == key) {
            values.push((key, Value::Number(r.value)));
        }
    }
    let temps = || readings.iter().filter(|r| r.kind == TEMPERATURE && r.value.is_finite());
    let lower = |s: &str| s.to_lowercase();
    let die = temps()
        .find(|r| lower(&r.label).contains("tdie") || lower(&r.label).contains("tctl"))
        .or_else(|| temps().find(|r| lower(&r.label).contains("cpu")))
        .or_else(|| temps().find(|r| lower(&r.sensor).starts_with("cpu")));
    if let Some(die) = die {
        values.push(("pc.cpu.temp".into(), Value::Number(die.value)));
    }
    values
}

/// Lowercase a-z, 0-9 and `_`, with runs of anything else as one `_`.
fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().map(|c| c.to_ascii_lowercase()) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_end_matches('_').to_string()
}

/// The Windows calls, each giving None (or an error) when it can't be made.
mod win {
    use std::ffi::c_void;
    use std::mem::{size_of, transmute, zeroed};

    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows_sys::Win32::System::Memory::{
        FILE_MAP_READ, MEMORY_BASIC_INFORMATION, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery,
    };
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetSystemTimes;

    use super::OnceLock;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// Idle, kernel and user time, in 100 ns units, summed over every core.
    pub fn cpu_times() -> Option<[u64; 3]> {
        let ticks = |t: FILETIME| (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64;
        unsafe {
            let (mut idle, mut kernel, mut user) = (zeroed(), zeroed(), zeroed());
            (GetSystemTimes(&mut idle, &mut kernel, &mut user) != 0).then(|| [ticks(idle), ticks(kernel), ticks(user)])
        }
    }

    /// Physical memory: total and available bytes.
    pub fn memory() -> Option<(u64, u64)> {
        unsafe {
            let mut status: MEMORYSTATUSEX = zeroed();
            status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
            (GlobalMemoryStatusEx(&mut status) != 0).then_some((status.ullTotalPhys, status.ullAvailPhys))
        }
    }

    /// A copy of HWiNFO's shared memory, if it's running with Shared Memory Support on.
    pub fn hwinfo() -> Result<Vec<u8>, String> {
        let missing = || "HWiNFO isn't running, or its Shared Memory Support is off".to_string();
        unsafe {
            let mapping = OpenFileMappingW(FILE_MAP_READ, 0, wide(r"Global\HWiNFO_SENS_SM2").as_ptr());
            if mapping.is_null() {
                return Err(missing());
            }
            let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
            CloseHandle(mapping); // the view keeps it open
            if view.Value.is_null() {
                return Err(missing());
            }
            let mut region: MEMORY_BASIC_INFORMATION = zeroed();
            let size = if VirtualQuery(view.Value, &mut region, size_of::<MEMORY_BASIC_INFORMATION>()) != 0 {
                region.RegionSize
            } else {
                0
            };
            let bytes = std::slice::from_raw_parts(view.Value as *const u8, size).to_vec();
            UnmapViewOfFile(view);
            Ok(bytes)
        }
    }

    type Init = unsafe extern "C" fn() -> i32;
    type Handle = unsafe extern "C" fn(u32, *mut *mut c_void) -> i32;
    type GetU32 = unsafe extern "C" fn(*mut c_void, *mut u32) -> i32;
    type GetTemp = unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> i32;
    type GetUse = unsafe extern "C" fn(*mut c_void, *mut [u32; 2]) -> i32;
    type GetMemory = unsafe extern "C" fn(*mut c_void, *mut [u64; 3]) -> i32;

    /// The first NVIDIA GPU, through NVML (nvml.dll, part of the driver), loaded once.
    pub struct Nvml {
        device: usize,
        temp: GetTemp,
        load: GetUse,
        power: GetU32,
        memory: GetMemory,
    }

    pub struct Gpu {
        pub temp: Option<u32>,
        pub load: Option<u32>,
        pub power_mw: Option<u32>,
        /// Total and used bytes.
        pub memory: Option<(u64, u64)>,
    }

    pub fn nvml() -> Result<&'static Nvml, &'static str> {
        static NVML: OnceLock<Result<Nvml, &'static str>> = OnceLock::new();
        NVML.get_or_init(|| unsafe { load_nvml() }).as_ref().map_err(|e| *e)
    }

    unsafe fn load_nvml() -> Result<Nvml, &'static str> {
        unsafe {
            let lib = ["nvml.dll", r"C:\Program Files\NVIDIA Corporation\NVSMI\nvml.dll"]
                .iter()
                .map(|path| LoadLibraryW(wide(path).as_ptr()))
                .find(|lib| !lib.is_null())
                .ok_or("no NVIDIA driver (nvml.dll)")?;
            let symbol = |name: &[u8]| GetProcAddress(lib, name.as_ptr()).ok_or("NVML is missing a function");
            let init: Init = transmute(symbol(b"nvmlInit_v2\0")?);
            let handle: Handle = transmute(symbol(b"nvmlDeviceGetHandleByIndex_v2\0")?);
            if init() != 0 {
                return Err("NVML didn't start");
            }
            let mut device = std::ptr::null_mut();
            if handle(0, &mut device) != 0 {
                return Err("no NVIDIA GPU");
            }
            Ok(Nvml {
                device: device as usize,
                temp: transmute(symbol(b"nvmlDeviceGetTemperature\0")?),
                load: transmute(symbol(b"nvmlDeviceGetUtilizationRates\0")?),
                power: transmute(symbol(b"nvmlDeviceGetPowerUsage\0")?),
                memory: transmute(symbol(b"nvmlDeviceGetMemoryInfo\0")?),
            })
        }
    }

    impl Nvml {
        /// Each reading that works (0 is NVML's success).
        pub fn read(&self) -> Gpu {
            let device = self.device as *mut c_void;
            unsafe {
                let (mut temp, mut load, mut power, mut memory) = (0u32, [0u32; 2], 0u32, [0u64; 3]);
                Gpu {
                    temp: ((self.temp)(device, 0, &mut temp) == 0).then_some(temp),
                    load: ((self.load)(device, &mut load) == 0).then_some(load[0]),
                    power_mw: ((self.power)(device, &mut power) == 0).then_some(power),
                    memory: ((self.memory)(device, &mut memory) == 0).then_some((memory[0], memory[2])),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn get<'a>(values: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
        values.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    #[test]
    fn claude_reply() {
        let reply: Json = serde_json::from_str(r#"{
            "five_hour": { "utilization": 30.0, "resets_at": "2026-10-14T10:50:00.397326+00:00" },
            "seven_day": { "utilization": 94.0, "resets_at": "2026-10-14T22:00:00.397357+00:00" },
            "seven_day_opus": null,
            "limits": [
                { "kind": "session", "percent": 30, "severity": "normal", "resets_at": "2026-10-14T10:50:00+00:00" },
                { "kind": "weekly_all", "percent": 94, "severity": "critical", "resets_at": "..." },
                { "kind": "weekly_scoped", "percent": 14, "severity": "normal", "scope": { "model": { "display_name": "Fable" } } },
                { "kind": "weekly_scoped", "percent": 3, "scope": { "model": {} } },
                { "kind": "something_new", "percent": 1 }
            ]
        }"#).unwrap();
        let v = claude_values(&reply);
        let at = |h, m| Utc.with_ymd_and_hms(2026, 10, 14, h, m, 0).unwrap();
        assert_eq!(get(&v, "claude.session"), Some(&Value::Number(30.0)));
        assert_eq!(get(&v, "claude.week"), Some(&Value::Number(94.0)));
        let Some(Value::Time(resets)) = get(&v, "claude.session_resets") else { panic!() };
        assert_eq!(resets.timestamp(), at(10, 50).timestamp());
        let Some(Value::Until(resets)) = get(&v, "claude.week_resets_in") else { panic!() };
        assert_eq!(resets.timestamp(), at(22, 0).timestamp());
        assert_eq!(get(&v, "claude.session_severity"), Some(&Value::Text("normal".into())));
        assert_eq!(get(&v, "claude.week_severity"), Some(&Value::Text("critical".into())));
        assert_eq!(get(&v, "claude.week.fable"), Some(&Value::Number(14.0)));
        assert_eq!(v.len(), 9);
        // anything missing is just left out
        assert!(claude_values(&serde_json::json!({ "five_hour": { "utilization": null } })).is_empty());
        assert!(claude_values(&serde_json::json!([])).is_empty());
    }

    #[test]
    fn retry_after_header() {
        let now = Utc.with_ymd_and_hms(2026, 10, 14, 10, 0, 0).unwrap();
        assert_eq!(retry_after("120", now), Some(Duration::from_secs(120)));
        assert_eq!(retry_after(" 0 ", now), Some(Duration::ZERO));
        assert_eq!(retry_after("Wed, 14 Oct 2026 10:05:00 GMT", now), Some(Duration::from_secs(300)));
        assert_eq!(retry_after("Wed, 14 Oct 2026 09:05:00 GMT", now), Some(Duration::ZERO)); // already past
        assert_eq!(retry_after("soon", now), None);
        assert_eq!(retry_after("", now), None);
        // a plugin's message names its status; the rest don't
        assert!(Failure::from("HTTP 429 from the API".to_string()).throttled());
        assert_eq!(Failure::from("503 Service Unavailable").status, Some(503));
        assert_eq!(Failure::from("exited with code 4290").status, None);
        assert!(!Failure::from("connection refused").throttled());
    }

    #[test]
    fn ha_states() {
        let states: Json = serde_json::from_str(r#"[
            { "entity_id": "sensor.living_room_temperature", "state": "21.5",
              "attributes": { "unit_of_measurement": "°C", "device_class": "temperature", "friendly_name": "Living room",
                              "Accuracy": 2, "options": ["a"], "extra": null, "on": true } },
            { "entity_id": "weather.home", "state": "partlycloudy", "attributes": { "temperature": 14 } },
            { "entity_id": "sensor.other", "state": "1" },
            { "entity_id": "sensor.down", "state": "unavailable", "attributes": {} }
        ]"#).unwrap();
        let wanted = ["sensor.living_room_temperature", "weather.home", "sensor.down", "sensor.gone"].map(String::from);
        let polled = ha_values(&states, &wanted);
        let v = &polled.values;
        assert_eq!(get(v, "ha.sensor.living_room_temperature"), Some(&Value::Number(21.5)));
        assert_eq!(get(v, "ha.sensor.living_room_temperature.unit"), Some(&Value::Text("°C".into())));
        assert_eq!(get(v, "ha.sensor.living_room_temperature.friendly_name"), Some(&Value::Text("Living room".into())));
        assert_eq!(get(v, "ha.sensor.living_room_temperature.accuracy"), Some(&Value::Number(2.0)));
        assert_eq!(get(v, "ha.sensor.living_room_temperature.on"), Some(&Value::Text("true".into())));
        assert!(get(v, "ha.sensor.living_room_temperature.options").is_none());
        assert!(get(v, "ha.sensor.living_room_temperature.extra").is_none());
        assert_eq!(get(v, "ha.weather.home"), Some(&Value::Text("partlycloudy".into())));
        assert_eq!(get(v, "ha.weather.home.temperature"), Some(&Value::Number(14.0)));
        assert_eq!(get(v, "ha.sensor.down"), Some(&Value::Text("unavailable".into())));
        assert!(get(v, "ha.sensor.other").is_none());
        assert_eq!(polled.notes, ["no entity sensor.gone"]);
    }

    #[test]
    fn hwinfo_memory() {
        // a header, two sensors and three readings, laid out as HWiNFO does
        let (sensor_size, reading_size) = (392usize, 460usize);
        let mut bytes = vec![0u8; 44 + 2 * sensor_size + 3 * reading_size];
        let put = |bytes: &mut Vec<u8>, at: usize, data: &[u8]| bytes[at..at + data.len()].copy_from_slice(data);
        put(&mut bytes, 0, b"HWiS");
        for (at, n) in [(20, 44), (24, sensor_size), (28, 2), (32, 44 + 2 * sensor_size), (36, reading_size), (40, 3)] {
            put(&mut bytes, at, &(n as u32).to_le_bytes());
        }
        put(&mut bytes, 44 + 136, b"CPU [#0]: AMD Ryzen 7 5800X: Enhanced");
        put(&mut bytes, 44 + sensor_size + 136, b"GPU [#0]: NVIDIA");
        let readings_at = 44 + 2 * sensor_size;
        for (i, (kind, sensor, label, value)) in
            [(1u32, 1u32, "GPU Temperature", 55.0f64), (1, 0, "CPU (Tctl/Tdie)", 61.5), (7, 0, "Total CPU Usage", 12.0)]
                .into_iter()
                .enumerate()
        {
            let at = readings_at + i * reading_size;
            put(&mut bytes, at, &kind.to_le_bytes());
            put(&mut bytes, at + 4, &sensor.to_le_bytes());
            put(&mut bytes, at + 140, label.as_bytes());
            put(&mut bytes, at + 284, &value.to_le_bytes());
        }
        let values = hwinfo_values(&hwinfo_readings(&bytes).unwrap());
        assert_eq!(get(&values, "hwinfo.cpu_0_amd_ryzen_7_5800x_enhanced.cpu_tctl_tdie"), Some(&Value::Number(61.5)));
        assert_eq!(get(&values, "hwinfo.gpu_0_nvidia.gpu_temperature"), Some(&Value::Number(55.0)));
        assert_eq!(get(&values, "pc.cpu.temp"), Some(&Value::Number(61.5)));
        assert_eq!(values.len(), 4);
        bytes[..4].copy_from_slice(b"DEAD");
        assert!(hwinfo_readings(&bytes).is_err());
    }

    /// LHM's data.json, with a node of each shape: 0.9.5 and 0.9.6 (RawValue as text, some in a Regional
    /// format with a decimal comma, and a PSU's time span in the exponent form .NET Framework writes), the
    /// newest (RawValue a number or null, data in bytes), before 0.9.5 (no RawValue, so throughput only as
    /// shown, scaled: the "Ethernet" hardware), and before 0.8.7 (no ids, no Type either: "Generic Memory").
    const LHM_DATA: &str = include_str!("../tests/lhm-data.json");

    fn lhm_tree(data: &str) -> Vec<(String, Value)> {
        lhm_values(&lhm_sensors(&serde_json::from_str(data).unwrap()))
    }

    #[test]
    fn lhm_data_json() {
        let v = lhm_tree(LHM_DATA);
        let n = |key: &str| match get(&v, key) {
            Some(Value::Number(n)) => *n,
            other => panic!("{key}: {other:?}"),
        };
        // the SuperIO chip, nested in the motherboard, is the hardware; a decimal comma reads as a point
        assert_eq!(n("lhm.nuvoton_nct6799d.voltage.vcore"), 1.35);
        assert_eq!(n("lhm.nuvoton_nct6799d.voltage.12v"), 12.192);
        assert_eq!(n("lhm.nuvoton_nct6799d.temperature.cpu"), 45.0);
        assert_eq!(n("lhm.nuvoton_nct6799d.fan.fan_1"), 1023.0);
        assert_eq!(n("lhm.nuvoton_nct6799d.control.fan_1"), 45.1);
        assert_eq!(n("lhm.amd_ryzen_9_7950x.temperature.core_tctl_tdie"), 45.3);
        assert_eq!(n("lhm.amd_ryzen_9_7950x.temperature.ccd1_tdie"), 41.0);
        assert_eq!(n("lhm.amd_ryzen_9_7950x.clock.core_1"), 5082.4);
        assert_eq!(n("lhm.amd_ryzen_9_7950x.load.core_max"), 21.9);
        assert_eq!(n("lhm.amd_ryzen_9_7950x.power.package"), 62.5);
        // before 0.9.5: the type from the group's name, the value from what's shown
        assert_eq!(n("lhm.generic_memory.load.memory"), 57.2);
        assert_eq!(n("lhm.generic_memory.data.memory_used"), 18.2);
        assert_eq!(n("lhm.nvidia_geforce_rtx_4090.temperature.gpu_core"), 52.0);
        assert_eq!(n("lhm.nvidia_geforce_rtx_4090.smalldata.gpu_memory_used"), 8123.0);
        // the second of two identical GPUs
        assert_eq!(n("lhm.nvidia_geforce_rtx_4090_2.temperature.gpu_core"), 48.5);
        assert_eq!(n("lhm.nvidia_geforce_rtx_4090_2.load.gpu_core"), 12.0);
        // a raw number wins over the shown text, which is in KB/s; data in bytes is GB, as before
        assert_eq!(n("lhm.samsung_ssd_990_pro_2tb.temperature.composite_temperature"), 39.85);
        assert_eq!(n("lhm.samsung_ssd_990_pro_2tb.throughput.read_rate"), 123456.5);
        assert_eq!(n("lhm.samsung_ssd_990_pro_2tb.data.data_read"), 2048.0);
        // a time span in seconds, short and long ("1.234568e+07": 7 digits, then the exponent)
        assert_eq!(n("lhm.corsair_hx1000i.timespan.uptime"), 3600.5);
        assert_eq!(n("lhm.corsair_hx1000i.timespan.total_uptime"), 12_345_680.0);
        // throughput shown scaled, back in B/s; the link speed in bit/s
        assert_eq!(n("lhm.ethernet.throughput.upload_speed"), 512.0 * 1024.0);
        assert_eq!(n("lhm.ethernet.throughput.download_speed"), 1.2 * 1024.0 * 1024.0);
        assert_eq!(n("lhm.ethernet.throughput.connection_speed"), 1e9);
        // no value ("-", " RPM", null, NaN): left out
        for key in ["lhm.nuvoton_nct6799d.fan.fan_2", "lhm.nvidia_geforce_rtx_4090.temperature.gpu_memory_junction",
                    "lhm.nvidia_geforce_rtx_4090.fan.gpu_fan_2", "lhm.samsung_ssd_990_pro_2tb.throughput.write_rate"] {
            assert!(get(&v, key).is_none(), "{key}");
        }
        // the CPU's own die temperature, not the motherboard's "CPU" nor a CCD's
        assert_eq!(n("pc.cpu.temp"), 45.3);
        assert_eq!(v.len(), 36);
        assert!(v.iter().all(|(k, _)| k == "pc.cpu.temp" || (k.starts_with("lhm.") && k.split('.').count() == 4)));
        // nothing above the hardware (the root, the computer) is a sensor
        assert!(v.iter().all(|(k, _)| !k.contains("desktop_jim") && !k.contains("sensor.")));
    }

    #[test]
    fn lhm_keys() {
        // two identical boards, each with a doubled sensor, an unnamed one, and a type LHM added later
        let board = |id: u32| format!(r##"{{ "Text": "Board", "HardwareId": "/lpc/x/{id}", "Children": [
            {{ "Text": "Temperatures", "Children": [
                {{ "Text": "Aux", "Type": "Temperature", "SensorId": "/lpc/x/{id}/temperature/0", "Value": "1.0 °C" }},
                {{ "Text": "Aux", "Type": "Temperature", "SensorId": "/lpc/x/{id}/temperature/1", "Value": "2.0 °C" }},
                {{ "Text": "Aux 2", "Type": "Temperature", "SensorId": "/lpc/x/{id}/temperature/2", "Value": "3.0 °C" }},
                {{ "Text": "#", "Type": "Temperature", "SensorId": "/lpc/x/{id}/temperature/3", "Value": "4.0 °C" }} ] }},
            {{ "Text": "Wobbles", "Children": [ {{ "Text": "Wobble", "Value": "5" }} ] }} ] }}"##);
        let tree = format!(r#"{{ "Text": "Sensor", "Children": [ {{ "Text": "PC", "Children": [ {}, {} ] }} ] }}"#, board(0), board(1));
        let v = lhm_tree(&tree);
        let keys: Vec<&str> = v.iter().map(|(k, _)| k.as_str()).collect();
        // the one named "#" by its id's end
        assert_eq!(keys, [
            "lhm.board.temperature.aux", "lhm.board.temperature.aux_2", "lhm.board.temperature.aux_2_2",
            "lhm.board.temperature.temperature_3", "lhm.board.wobble.wobble",
            "lhm.board_2.temperature.aux", "lhm.board_2.temperature.aux_2", "lhm.board_2.temperature.aux_2_2",
            "lhm.board_2.temperature.temperature_3", "lhm.board_2.wobble.wobble",
        ]);
        assert_eq!(get(&v, "lhm.board.temperature.aux_2"), Some(&Value::Number(2.0)));
        assert_eq!(get(&v, "lhm.board.temperature.aux_2_2"), Some(&Value::Number(3.0)));
        // the same tree always gives the same keys
        assert_eq!(lhm_tree(&tree), v);
        // ...even when a sensor has no reading for a while: it's left out, and holds its key
        let gap = tree.replacen(r#""Value": "1.0 °C""#, r#""Value": "-""#, 1);
        let without = lhm_tree(&gap);
        assert_eq!(get(&without, "lhm.board.temperature.aux"), None);
        assert_eq!(get(&without, "lhm.board.temperature.aux_2"), Some(&Value::Number(2.0)));
        assert_eq!(get(&without, "lhm.board.temperature.aux_2_2"), Some(&Value::Number(3.0)));
        assert_eq!(without.len(), v.len() - 1);

        // names with nothing in a-z or 0-9 (a NIC Windows names in Japanese, a renamed sensor): by their id,
        // else as "hardware" and "sensor"; accents are dropped
        let tree = r#"{ "Text": "Sensor", "Children": [ { "Text": "PC", "Children": [
            { "Text": "イーサネット", "HardwareId": "/nic/{9F1B2C3D-0000-4000-8000-00AA00BB00CC}", "Children": [
                { "Text": "Throughput", "Children": [
                    { "Text": "Débit", "Type": "Throughput", "SensorId": "/nic/{9F1B2C3D-0000-4000-8000-00AA00BB00CC}/throughput/7", "Value": "5.0 B/s" },
                    { "Text": "温度", "Type": "Throughput", "SensorId": "/nic/{9F1B2C3D-0000-4000-8000-00AA00BB00CC}/throughput/8", "Value": "6.0 B/s" } ] } ] },
            { "Text": "Ѳ", "Children": [ { "Text": "Temperatures", "Children": [ { "Text": "湿度", "Value": "7.0 °C" },
                                                                        { "Text": "Lüfter Ærø", "Value": "8.0 °C" } ] } ] } ] } ] }"#;
        let keys: Vec<String> = lhm_tree(tree).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, [
            "lhm.nic_9f1b2c3d_0000_4000_8000_00aa00bb00cc.throughput.debit",
            "lhm.nic_9f1b2c3d_0000_4000_8000_00aa00bb00cc.throughput.throughput_8",
            "lhm.hardware.temperature.sensor", "lhm.hardware.temperature.lufter_aero",
        ]);
        // older names for the groups
        let types: Vec<String> = ["Voltages", "Capacities", "Times", "Noise Levels", "Humidity Levels", "Fans", "Data"]
            .into_iter().map(lhm_type).collect();
        assert_eq!(types, ["voltage", "energy", "timespan", "noise", "humidity", "fan", "data"]);
        assert_eq!(slug("TimeSpan"), "timespan");
        assert_eq!(slug("SmallData"), "smalldata");
    }

    #[test]
    fn lhm_readings() {
        for (text, want) in [("45.0 °C", Some(45.0)), ("45,0 °C", Some(45.0)), ("  7.4 %", Some(7.4)), ("1023 RPM", Some(1023.0)),
                             ("−5,5 °C", Some(-5.5)), ("-0.25 V", Some(-0.25)), ("12.", Some(12.0)),
                             ("1:02:03", Some(3723.0)), ("2:01:02:03,5", Some(176_523.5)), ("0:00:07.25", Some(7.25)),
                             ("-", None), ("", None), (" °C", None), ("NaN RPM", None), ("∞ W", None), (".5", None)] {
            assert_eq!(lhm_reading(text), want, "{text:?}");
        }
        // Regional formats, as .NET 10 (ICU) writes them: Arabic's decimal point, and bidi marks around the
        // sign (ar-SA, fa-IR, he-IL, ps-AF); fa-IR's "/" on .NET Framework
        for (text, want) in [("45٫3 °C", 45.3), ("1٫250 V", 1.25), ("\u{61c}-5٫5 °C", -5.5), ("\u{200e}\u{2212}5٫5 °C", -5.5),
                             ("\u{200e}-5.5 °C", -5.5), ("\u{200f}-12٫5", -12.5), ("\u{200e}-\u{200e}5٫5 °C", -5.5),
                             ("45/3 °C", 45.3), ("-5/5 °C", -5.5), ("1/250 V", 1.25)] {
            assert_eq!(lhm_reading(text), Some(want), "{text:?}");
        }
        // a big time span in seconds, in exponent form (.NET Framework, past 1e7)
        for (text, want) in [("1.234568e+07", 12_345_680.0), ("1,234568e+07", 12_345_680.0), ("1E+10", 1e10),
                             ("2.5e-05", 2.5e-5), ("1٫234568e\u{200e}+07", 12_345_680.0), ("2/5e-05", 2.5e-5), ("5e", 5.0)] {
            assert_eq!(lhm_reading(text), Some(want), "{text:?}");
        }
        // throughput as shown before 0.9.5: back in B/s, the link speed in bit/s
        for (text, want) in [("512,0 KB/s", 524_288.0), ("2.0 MB/s", 2_097_152.0), ("1.0 GB/s", 1_073_741_824.0),
                             ("123.0 B/s", 123.0), ("1Gbps", 1e9), ("100Mbps", 1e8), ("900 bps", 900.0),
                             ("9.5 Mbps", 9.5 * 1_048_576.0), ("2.5 Gbps", 2.5 * 1_073_741_824.0), ("8123.0 MB", 8123.0)] {
            assert_eq!(lhm_reading(text), Some(want), "{text:?}");
        }
        // shown in Fahrenheit: back to Celsius
        assert!(lhm_reading("113.0 °F").is_some_and(|c| (c - 45.0).abs() < 1e-9));
        let node = |raw: Json, shown: &str| lhm_value(&serde_json::json!({ "RawValue": raw, "Value": shown }));
        assert_eq!(node(serde_json::json!(39.85), "39.9 °C"), Some(39.85));
        assert_eq!(node(serde_json::json!("39,9 °C"), "102.0 °F"), Some(39.9));
        assert_eq!(node(Json::Null, "-"), None);
        assert_eq!(node(serde_json::json!("NaN"), "NaN °C"), None);
        assert_eq!(lhm_value(&serde_json::json!({ "Value": "41,5 °C" })), Some(41.5));
        // the exponent form, with the exact time span shown beside it
        assert_eq!(node(serde_json::json!("1.234568e+07"), "142:21:21:18"), Some(12_345_680.0));
        // the newest code's data is in bytes: GB, as 0.9.6 gave it
        let data = |raw: Json| lhm_value(&serde_json::json!({ "Type": "Data", "RawValue": raw, "Value": "x" }));
        assert_eq!(data(serde_json::json!(19_541_196_800.0_f64)), Some(19_541_196_800.0 / 1_073_741_824.0));
        assert_eq!(data(serde_json::json!("18,2 GB")), Some(18.2));
        assert_eq!(lhm_base(""), LHM_URL);
        assert_eq!(lhm_base(" http://127.0.0.1:8085/ "), "http://127.0.0.1:8085");
        assert_eq!(lhm_base("192.168.1.5:8086"), "http://192.168.1.5:8086");
        // the data's own address, as a browser shows it
        assert_eq!(lhm_base("http://127.0.0.1:8085/data.json"), "http://127.0.0.1:8085");
        assert_eq!(lhm_base("192.168.1.5:8086/Data.JSON/"), "http://192.168.1.5:8086");
        assert_eq!(lhm_base("/data.json"), LHM_URL);
        assert_eq!(lhm_base("http://host/lhm/data.json"), "http://host/lhm");
    }

    #[test]
    fn lhm_fetch_options() {
        // on this PC: never through a proxy; localhost at 127.0.0.1 only
        let fetch = |url: &str| {
            let f = lhm_fetch(&lhm_base(url));
            (f.direct, f.ipv4, f.timeout, f.limit)
        };
        let (timeout, limit) = (LHM_TIMEOUT, LHM_LIMIT);
        assert_eq!(fetch(""), (true, false, timeout, limit));
        assert_eq!(fetch("http://127.0.0.1:8085"), (true, false, timeout, limit));
        assert_eq!(fetch("127.4.5.6:8085"), (true, false, timeout, limit));
        assert_eq!(fetch("http://LocalHost:8085/data.json"), (true, true, timeout, limit));
        assert_eq!(fetch("http://[::1]:8085"), (true, false, timeout, limit));
        // elsewhere, as the environment says
        assert_eq!(fetch("http://192.168.1.5:8085"), (false, false, timeout, limit));
        assert_eq!(fetch("http://pc.local:8085"), (false, false, timeout, limit));
        assert_eq!(fetch("http://localhost.example.com"), (false, false, timeout, limit));
        assert!(LHM_LIMIT < WEB.limit);
    }

    /// A tree with one CPU (`id`, `icon`: how it's marked) holding these temperatures, after a GPU's.
    fn cpu_tree(id: Option<&str>, icon: &str, temps: &[(&str, f64)]) -> String {
        let sensors: Vec<String> = temps.iter().enumerate().map(|(i, (name, t))| match id {
            Some(id) => format!(r#"{{ "Text": "{name}", "SensorId": "{id}/temperature/{i}", "Type": "Temperature", "Value": "{t} °C" }}"#),
            None => format!(r#"{{ "Text": "{name}", "Value": "{t} °C", "Children": [] }}"#),
        }).collect();
        let hardware_id = match id {
            Some(id) => format!(r#""HardwareId": "{id}","#),
            None => String::new(),
        };
        format!(r#"{{ "Text": "Sensor", "Children": [ {{ "Text": "PC", "Children": [
            {{ "Text": "GPU", "HardwareId": "/gpu-nvidia/0", "Children": [ {{ "Text": "Temperatures", "Children": [
                {{ "Text": "GPU Core", "SensorId": "/gpu-nvidia/0/temperature/0", "Type": "Temperature", "Value": "99.0 °C" }} ] }} ] }},
            {{ "Text": "CPU", {hardware_id} "ImageURL": "images_icon/{icon}", "Children": [
                {{ "Text": "Temperatures", "Children": [ {} ] }} ] }} ] }} ] }}"#, sensors.join(","))
    }

    #[test]
    fn lhm_cpu_temperature() {
        let temp = |id, icon, temps: &[(&str, f64)]| get(&lhm_tree(&cpu_tree(id, icon, temps)), "pc.cpu.temp").cloned();
        let amd = Some("/amdcpu/0");
        let intel = Some("/intelcpu/0");
        // AMD: Tctl/Tdie, wherever it is; when shown apart, Tdie, then Tctl
        assert_eq!(temp(amd, "cpu.png", &[("CCD1 (Tdie)", 41.0), ("Core (Tctl/Tdie)", 45.3)]), Some(Value::Number(45.3)));
        assert_eq!(temp(amd, "cpu.png", &[("Core (Tctl)", 55.3), ("Core (Tdie)", 45.3), ("CCD1 (Tdie)", 41.0)]), Some(Value::Number(45.3)));
        assert_eq!(temp(amd, "cpu.png", &[("CCD1 (Tdie)", 41.0), ("Core (Tctl)", 55.3)]), Some(Value::Number(55.3)));
        // Intel: the package
        assert_eq!(temp(intel, "cpu.png", &[("CPU Core #1", 50.0), ("CPU Package", 58.0), ("Core Max", 61.0)]),
                   Some(Value::Number(58.0)));
        // else Core Max, else Core Average, else the CPU's first
        assert_eq!(temp(intel, "cpu.png", &[("CPU Core #1", 50.0), ("Core Average", 52.0), ("Core Max", 61.0)]),
                   Some(Value::Number(61.0)));
        assert_eq!(temp(intel, "cpu.png", &[("CPU Core #1", 50.0), ("Core Average", 52.0)]), Some(Value::Number(52.0)));
        assert_eq!(temp(intel, "cpu.png", &[("CPU Core #1", 50.0), ("CPU Core #2", 51.0)]), Some(Value::Number(50.0)));
        // never the GPU's, nor any other hardware's
        assert_eq!(temp(intel, "cpu.png", &[]), None);
        assert_eq!(temp(Some("/lpc/nct6799d/0"), "cpu.png", &[("CPU", 47.0)]), None);
        // with no ids at all (before LHM, or a stripped-down tree), the CPU is told by its icon
        assert_eq!(temp(None, "cpu.png", &[("Core (Tctl/Tdie)", 44.0)]), Some(Value::Number(44.0)));
        assert_eq!(temp(None, "chip.png", &[("CPU", 47.0)]), None);
        // with sensor ids but no hardware ids (0.8.7 to 0.9.4), by the sensor's id
        let old = cpu_tree(amd, "chip.png", &[("Core (Tctl/Tdie)", 43.0)]).replace(r#""HardwareId": "/amdcpu/0","#, "");
        assert!(!old.contains("HardwareId\": \"/amdcpu"));
        assert_eq!(get(&lhm_tree(&old), "pc.cpu.temp"), Some(&Value::Number(43.0)));

        // HWiNFO's comes first, when it gave one; LHM's sensors come through all the same
        let lhm = lhm_tree(&cpu_tree(amd, "cpu.png", &[("Core (Tctl/Tdie)", 45.3)]));
        let mut values = vec![("pc.cpu.temp".to_string(), Value::Number(61.5)), ("pc.cpu.load".to_string(), Value::Number(9.0))];
        add_lhm(&mut values, lhm.clone());
        assert_eq!(values.iter().filter(|(k, _)| k == "pc.cpu.temp").count(), 1);
        assert_eq!(get(&values, "pc.cpu.temp"), Some(&Value::Number(61.5)));
        assert_eq!(get(&values, "lhm.cpu.temperature.core_tctl_tdie"), Some(&Value::Number(45.3)));
        let mut values = vec![("pc.cpu.load".to_string(), Value::Number(9.0))];
        add_lhm(&mut values, lhm);
        assert_eq!(get(&values, "pc.cpu.temp"), Some(&Value::Number(45.3)));
    }

    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Instant;

    /// A local web server on a free port that gives each connection the next reply, and reports each
    /// request line.
    fn serve(replies: Vec<String>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let mut header = String::new();
                while reader.read_line(&mut header).unwrap_or(0) > 2 {
                    header.clear();
                }
                let _ = tx.send(line.trim_end().to_string());
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        (url, rx)
    }

    fn http(status: &str, body: &str) -> String {
        format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len())
    }

    #[test]
    fn lhm_over_http() {
        let (url, requests) = serve(vec![http("200 OK", LHM_DATA), http("200 OK", "<html>LHM</html>"),
                                         http("404 Not Found", ""), http("200 OK", "{}"), http("401 Unauthorized", "")]);
        let (values, note) = lhm(&format!("{url}/")).unwrap();
        assert_eq!(requests.recv().unwrap(), "GET /data.json HTTP/1.1");
        assert_eq!(get(&values, "pc.cpu.temp"), Some(&Value::Number(45.3)));
        assert_eq!((values.len(), note), (36, None));
        let note = lhm(&url).unwrap_err();
        assert!(note.starts_with(&format!("Libre Hardware Monitor isn't answering at {url} (its reply isn't sensor data)")), "{note}");
        assert!(note.ends_with("check that it's running, with Options > Remote Web Server on"), "{note}");
        assert!(lhm(&url).unwrap_err().contains("(HTTP 404 Not Found)"));
        assert!(lhm(&url).unwrap_err().contains("(its reply isn't sensor data)"));
        assert!(lhm(&url).unwrap_err().contains("(it asks for a password"));
    }

    #[test]
    fn lhm_address_and_size() {
        // the data's own address asks for /data.json once
        let (url, requests) = serve(vec![http("200 OK", LHM_DATA)]);
        assert_eq!(lhm(&format!("{url}/data.json")).unwrap().0.len(), 36);
        assert_eq!(requests.recv().unwrap(), "GET /data.json HTTP/1.1");

        // a reply far bigger than LHM's is not read
        let big = format!(r#"{{ "Children": [], "pad": "{}" }}"#, "x".repeat(LHM_LIMIT as usize));
        let (url, _requests) = serve(vec![http("200 OK", &big)]);
        let note = lhm(&url).unwrap_err();
        assert!(note.contains("(its reply is over 4 MB, far more than LHM's)"), "{note}");

        // past LHM_MAX_SENSORS, the rest are left out, with a note
        let sensors: Vec<String> = (0..LHM_MAX_SENSORS + 5)
            .map(|i| format!(r#"{{ "Text": "S{i}", "Type": "Load", "SensorId": "/x/0/load/{i}", "Value": "{i}.0 %" }}"#))
            .collect();
        let tree = format!(r#"{{ "Text": "Sensor", "Children": [ {{ "Text": "PC", "Children": [ {{ "Text": "X", "HardwareId": "/x/0",
            "Children": [ {{ "Text": "Load", "Children": [ {} ] }} ] }} ] }} ] }}"#, sensors.join(","));
        let (url, _requests) = serve(vec![http("200 OK", &tree)]);
        let (values, note) = lhm(&url).unwrap();
        assert_eq!(values.len(), LHM_MAX_SENSORS);
        assert_eq!(get(&values, &format!("lhm.x.load.s{}", LHM_MAX_SENSORS - 1)), Some(&Value::Number((LHM_MAX_SENSORS - 1) as f64)));
        let note = note.unwrap();
        assert!(note.contains(&format!("lists {} sensors: only the first {LHM_MAX_SENSORS} are read", LHM_MAX_SENSORS + 5)), "{note}");
    }

    #[test]
    fn lhm_not_running_is_a_note() {
        // nothing listening: the pc poll still works, with a note, and isn't held up
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");
        let source: Source = serde_json::from_value(serde_json::json!({ "type": "pc", "lhm": true, "lhm_url": url })).unwrap();
        let start = Instant::now();
        let polled = poll(&source, &mut State::default()).unwrap_or_else(|e| panic!("failed: {e}"));
        assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
        let note = format!("Libre Hardware Monitor isn't answering at {url} (");
        assert_eq!(polled.notes.iter().filter(|n| n.starts_with(&note)).count(), 1, "{:?}", polled.notes);
        assert!(get(&polled.values, "pc.ram.total_gb").is_some() && get(&polled.values, "pc.cpu.load").is_some());
        assert!(polled.values.iter().all(|(k, _)| !k.starts_with("lhm.")));
        // at localhost too: 127.0.0.1 only, so the reason shows before the time runs out
        let url = format!("http://localhost:{port}");
        let start = Instant::now();
        let note = lhm(&url).unwrap_err();
        assert!(start.elapsed() < LHM_TIMEOUT, "{:?}", start.elapsed());
        assert!(note.contains(&format!("isn't answering at {url} (nothing is listening there)")), "{note}");
    }

    #[test]
    fn lhm_silent_server_times_out() {
        // a server that takes the connection and never answers is given up on after LHM_TIMEOUT
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (hold, held) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let connection = listener.accept();
            let _ = held.recv(); // until the test is done
            drop(connection);
        });
        let start = Instant::now();
        let note = lhm(&url).unwrap_err();
        let took = start.elapsed();
        drop(hold);
        assert!(took >= Duration::from_millis(2500) && took < Duration::from_millis(4500), "{took:?}");
        assert!(note.contains(&format!("isn't answering at {url} (no reply in 3 s)")), "{note}");
    }
}
