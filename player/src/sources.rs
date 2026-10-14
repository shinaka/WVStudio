//! Where live values come from (docs/data.md): Claude plan usage, the PC's own sensors (Windows, NVIDIA,
//! HWiNFO) and Home Assistant. Each poll returns its values, or what went wrong; tokens never appear in
//! either.

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

pub fn poll(source: &Source, state: &mut State) -> Result<Polled, String> {
    match source.kind.as_str() {
        "claude" => Ok(Polled { values: claude()?, notes: Vec::new() }),
        "pc" => Ok(pc(source.hwinfo, state)),
        "homeassistant" => homeassistant(source),
        "time" => Ok(Polled { values: Vec::new(), notes: Vec::new() }), // always there, never polled
        other => match crate::catalog::manifest(other) {
            Some(manifest) => crate::plugin::poll(source, &manifest),
            None => Err(format!("unknown source type {other:?}")),
        },
    }
}

/// A GET, as text. Errors say what failed (the status, say), never the headers.
fn get(url: &str, headers: &[(&str, &str)]) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into();
    let mut request = agent.get(url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut reply = request.call().map_err(|e| e.to_string())?;
    reply.body_mut().with_config().limit(64 << 20).read_to_string().map_err(|e| e.to_string())
}

const USAGE: &str = "https://api.anthropic.com/api/oauth/usage";

/// Claude plan usage, from the endpoint Claude Code's /usage uses, with Claude Code's own login. The
/// login is only read: Claude Code renews it whenever it runs.
fn claude() -> Result<Vec<(String, Value)>, String> {
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
                             ("User-Agent", "wvplay")])?;
    let usage: Json = serde_json::from_str(&reply).map_err(|_| "the usage reply isn't JSON")?;
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

fn homeassistant(source: &Source) -> Result<Polled, String> {
    let url = source.url.trim().trim_end_matches('/');
    if url.is_empty() {
        return Err("no URL set".into());
    }
    if source.token.is_empty() {
        return Err("no token set".into());
    }
    let reply = get(&format!("{url}/api/states"), &[("Authorization", &format!("Bearer {}", source.token))])?;
    let states: Json = serde_json::from_str(&reply).map_err(|_| "the states reply isn't JSON")?;
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
/// `hwinfo`) everything HWiNFO reads.
fn pc(hwinfo: bool, state: &mut State) -> Polled {
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

    if hwinfo {
        match win::hwinfo().and_then(|bytes| hwinfo_readings(&bytes)) {
            Ok(readings) => values.extend(hwinfo_values(&readings)),
            Err(e) => notes.push(e),
        }
    }
    Polled { values, notes }
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
}
