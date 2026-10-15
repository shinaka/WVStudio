//! The source catalog (docs/sources.md): every source describes itself with a manifest, so the studio
//! builds its UI from these. The built-ins' manifests are compiled in; plugins bring their own in
//! `plugin.json`, read from the plugins folder at start-up and on rescan.

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use crate::player::log;

/// One source's manifest. `run` is only ever a plugin's, and never leaves the player.
#[derive(Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub interval: Interval,
    #[serde(default)]
    pub settings: Vec<Setting>,
    #[serde(default)]
    pub provides: Vec<Provides>,
    #[serde(default, skip_serializing)]
    pub run: Option<Run>,
}

/// The longest interval, in seconds: a day. A manifest can't ask for more, and the player never waits
/// longer, whatever data.json says.
pub const LONGEST_INTERVAL: f64 = 86_400.0;

#[derive(Clone, Serialize, Deserialize)]
pub struct Interval {
    pub default: f64,
    pub min: f64,
}

impl Default for Interval {
    fn default() -> Interval {
        Interval { default: 60.0, min: 5.0 }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Setting {
    pub key: String,
    pub label: String,
    /// text, number, bool, choice, list or password.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<String>>,
    /// A line of help under the control, for the studio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Grey text in the empty control, for the studio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// A number box's least and greatest, for the studio's arrows: hints, not checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

impl Setting {
    /// What a source that doesn't set this gets: the default, or for a choice without one, its first
    /// choice (as the studio saves).
    pub fn fallback(&self) -> Option<Json> {
        match (&self.default, &self.choices) {
            (Some(d), _) => Some(d.clone()),
            (None, Some(choices)) if self.kind == "choice" => choices.first().map(|c| Json::from(c.as_str())),
            _ => None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Provides {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// number, text or time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// A family of keys, like `hwinfo.*`, filled in by the live values.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pattern: bool,
}

/// How a plugin runs: its command line, in its folder, in `poll` or `stream` mode.
#[derive(Clone, Serialize, Deserialize)]
pub struct Run {
    pub command: Vec<String>,
    #[serde(default = "poll")]
    pub mode: String,
    /// The plugin's folder: where the command runs. Filled in on discovery, not in the file.
    #[serde(skip)]
    pub dir: PathBuf,
}

fn poll() -> String {
    "poll".into()
}

impl Manifest {
    pub fn setting(&self, key: &str) -> Option<&Setting> {
        self.settings.iter().find(|s| s.key == key)
    }

    /// Whether this is a plugin that keeps running, rather than one run per interval.
    pub fn streams(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.mode == "stream")
    }
}

const BUILTINS: &str = include_str!("builtins.json");

/// The compiled-in manifests: claude, pc, homeassistant and time.
pub fn builtins() -> Vec<Manifest> {
    serde_json::from_str(BUILTINS).expect("builtins.json is checked by a test")
}

/// Whether `id` is a built-in source's.
pub fn is_builtin(id: &str) -> bool {
    static IDS: OnceLock<Vec<String>> = OnceLock::new();
    IDS.get_or_init(|| builtins().into_iter().map(|m| m.id).collect()).iter().any(|b| b == id)
}

/// Why a plugin can't have `id`, if it can't: a built-in source's id, the first part of keys a
/// built-in sets (`ha`, `hwinfo`, `lhm`...), whose values the two would overwrite, or another
/// plugin's id in `found`.
fn taken(id: &str, found: &Catalog) -> Option<String> {
    let builtins = builtins();
    if let Some(b) = builtins.iter().find(|b| b.id == id) {
        return Some(format!("the id {id:?} is the built-in {} source's: choose another", b.name));
    }
    let root = |key: &str| key.split(['.', '*']).next().unwrap_or("").to_string();
    if let Some(b) = builtins.iter().find(|b| b.provides.iter().any(|p| root(&p.key) == id)) {
        return Some(format!("the id {id:?} is taken: the built-in {} source sets keys starting with \"{id}.\"", b.name));
    }
    let other = found.get(id)?.run.as_ref().map(|r| r.dir.display().to_string()).unwrap_or_default();
    Some(format!("the plugin in {other} already has the id {id:?}"))
}

/// The manifests the player knows, built-ins first, then plugins in folder order.
#[derive(Clone)]
pub struct Catalog {
    pub manifests: Vec<Manifest>,
    /// What went wrong reading plugin folders, for the rescan reply.
    pub problems: Vec<String>,
}

impl Catalog {
    pub fn get(&self, id: &str) -> Option<&Manifest> {
        self.manifests.iter().find(|m| m.id == id)
    }

    /// The manifests as the API shows them: without `run`, each with its `status`.
    pub fn json(&self, status: impl Fn(&Manifest) -> Json) -> Json {
        let list: Vec<Json> = self
            .manifests
            .iter()
            .map(|m| {
                let mut entry = serde_json::to_value(m).unwrap_or(Json::Null);
                entry["status"] = status(m);
                entry["plugin"] = json!(m.run.is_some());
                entry
            })
            .collect();
        json!({ "sources": list })
    }
}

fn catalog() -> &'static RwLock<Catalog> {
    static CATALOG: OnceLock<RwLock<Catalog>> = OnceLock::new();
    CATALOG.get_or_init(|| RwLock::new(Catalog { manifests: builtins(), problems: Vec::new() }))
}

/// The catalog as it is now: a copy, so callers never hold the lock.
pub fn current() -> Catalog {
    catalog().read().unwrap().clone()
}

pub fn manifest(id: &str) -> Option<Manifest> {
    catalog().read().unwrap().get(id).cloned()
}

/// Reads the plugins in `dir` (each a folder with a plugin.json) into the catalog, after the
/// built-ins. Broken ones are logged and left out. Returns the catalog it made.
pub fn rescan(dir: &Path) -> Catalog {
    let found = scan(dir);
    for problem in &found.problems {
        log(format_args!("plugins: {problem}"));
    }
    *catalog().write().unwrap() = found.clone();
    found
}

/// The built-ins plus the plugins in `dir`, with what was wrong with the rest.
fn scan(dir: &Path) -> Catalog {
    let mut found = Catalog { manifests: builtins(), problems: Vec::new() };
    let mut folders: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join("plugin.json").is_file()).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            found.problems.push(format!("couldn't read {}: {e}", dir.display()));
            Vec::new()
        }
    };
    folders.sort();
    for folder in folders {
        match read_plugin(&folder).and_then(|m| taken(&m.id, &found).map_or(Ok(m), Err)) {
            Ok(m) => found.manifests.push(m),
            Err(e) => found.problems.push(format!("{}: {e}", folder.display())),
        }
    }
    found
}

/// A plugin's manifest from its folder, checked: it needs an id fit for keys and a command.
fn read_plugin(folder: &Path) -> Result<Manifest, String> {
    let file = folder.join("plugin.json");
    let bytes = std::fs::read(&file).map_err(|e| format!("couldn't read plugin.json: {e}"))?;
    let mut m: Manifest = serde_json::from_slice(&bytes).map_err(|e| format!("plugin.json isn't valid: {e}"))?;
    check(&m)?;
    // a plugin may only set keys under its id, so that's all it can promise (pc's hwinfo.* is a built-in's)
    for p in &m.provides {
        if !p.key.starts_with(&format!("{}.", m.id)) {
            return Err(format!("provides {:?}, which isn't under {}.", p.key, m.id));
        }
    }
    let run = m.run.as_mut().ok_or("plugin.json has no \"run\" section")?;
    if run.command.is_empty() {
        return Err("\"run\" has no command".into());
    }
    if run.mode != "poll" && run.mode != "stream" {
        return Err(format!("\"run\" mode should be \"poll\" or \"stream\", not {:?}", run.mode));
    }
    run.dir = folder.to_path_buf();
    Ok(m)
}

/// What every manifest needs: an id fit for keys, and well-formed settings.
pub fn check(m: &Manifest) -> Result<(), String> {
    let id_ok = !m.id.is_empty()
        && m.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if !id_ok {
        return Err(format!("the id {:?} should be lowercase letters, digits, _ or -", m.id));
    }
    if m.name.trim().is_empty() {
        return Err("the manifest has no name".into());
    }
    if ![m.interval.min, m.interval.default].iter().all(|i| (1.0..=LONGEST_INTERVAL).contains(i)) {
        return Err(format!("the interval's min and default should be numbers from 1 to {LONGEST_INTERVAL} (a day)"));
    }
    let mut keys = std::collections::HashSet::new();
    for s in &m.settings {
        match s.kind.as_str() {
            "text" | "number" | "bool" | "list" | "password" => {}
            "choice" if s.choices.as_ref().is_some_and(|c| !c.is_empty()) => {}
            "choice" => return Err(format!("setting {:?} is a choice with no choices", s.key)),
            other => return Err(format!("setting {:?} has an unknown type {other:?}", s.key)),
        }
        // these sit beside the settings in data.json; any other key is the plugin's own (see data::Source)
        if s.key.is_empty() || ["type", "enabled", "interval"].contains(&s.key.as_str()) {
            return Err(format!("setting {:?} can't have that key: type, enabled and interval are the source's own", s.key));
        }
        // the studio's own: `<key>_set` tells the page whether a password is saved, without the password
        if s.key.ends_with("_set") {
            return Err(format!("setting {:?} can't have that key: the studio uses keys ending in _set to say a password is set", s.key));
        }
        if !keys.insert(s.key.as_str()) {
            return Err(format!("setting {:?} is there twice: each setting needs a key of its own", s.key));
        }
        if let (Some(min), Some(max)) = (s.min, s.max)
            && min > max
        {
            return Err(format!("setting {:?} has a min above its max", s.key));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_are_sound() {
        let all = builtins();
        let ids: Vec<&str> = all.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["claude", "pc", "homeassistant", "time"]);
        for m in &all {
            check(m).unwrap_or_else(|e| panic!("{}: {e}", m.id));
            assert!(m.run.is_none());
            assert!(!m.description.is_empty());
        }
        let ha = all.iter().find(|m| m.id == "homeassistant").unwrap();
        assert_eq!(ha.setting("token").unwrap().kind, "password");
        assert_eq!(ha.setting("entities").unwrap().kind, "list");
        assert!(ha.provides.iter().any(|p| p.key == "ha.*" && p.pattern));
        let pc = all.iter().find(|m| m.id == "pc").unwrap();
        assert_eq!(pc.setting("hwinfo").unwrap().kind, "bool");
        assert!(pc.provides.iter().any(|p| p.key == "hwinfo.*" && p.pattern));
        // Libre Hardware Monitor: off, at the address the player falls back to
        let lhm = pc.setting("lhm").unwrap();
        assert_eq!((lhm.kind.as_str(), &lhm.default), ("bool", &Some(json!(false))));
        let url = pc.setting("lhm_url").unwrap();
        assert_eq!((url.kind.as_str(), &url.default), ("text", &Some(json!(crate::sources::LHM_URL))));
        assert_eq!(crate::data::Source::default().lhm_url, crate::sources::LHM_URL);
        assert!(pc.provides.iter().any(|p| p.key == "lhm.*" && p.pattern));
        let temp = pc.provides.iter().find(|p| p.key == "pc.cpu.temp").unwrap();
        assert!(temp.label.contains("HWiNFO") && temp.label.contains("Libre Hardware Monitor"));
        for key in ["pc.cpu.load", "pc.cpu.temp", "pc.ram.used_gb", "pc.gpu.temp", "pc.gpu.vram_total_gb"] {
            assert!(pc.provides.iter().any(|p| p.key == key), "{key}");
        }
        let claude = all.iter().find(|m| m.id == "claude").unwrap();
        assert_eq!(claude.interval.min, 60.0);
        for key in ["claude.session", "claude.session_resets", "claude.session_resets_in", "claude.week_severity", "claude.week.*"] {
            assert!(claude.provides.iter().any(|p| p.key == key), "{key}");
        }
        // the catalog JSON has no run section, and each entry has a status
        let catalog = Catalog { manifests: all, problems: Vec::new() };
        let j = catalog.json(|m| json!({ "enabled": m.id == "time" }));
        let sources = j["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 4);
        assert!(sources.iter().all(|s| s.get("run").is_none() && s["status"].is_object()));
        assert_eq!(sources[3]["status"]["enabled"], json!(true));
        assert_eq!(sources[0]["settings"], json!([]));
    }

    #[test]
    fn plugin_manifests() {
        let m: Manifest = serde_json::from_str(r#"{ "id": "weather", "name": "Weather",
            "settings": [ { "key": "units", "label": "Units", "type": "choice", "choices": ["c", "f"], "default": "c" } ],
            "provides": [ { "key": "weather.temp", "label": "T", "unit": "°", "kind": "number" },
                          { "key": "weather.*", "label": "More", "pattern": true } ],
            "run": { "command": ["python", "weather.py"] } }"#).unwrap();
        check(&m).unwrap();
        assert_eq!(m.interval.default, 60.0);
        assert_eq!(m.run.as_ref().unwrap().mode, "poll");
        assert!(!m.streams());
        let j = serde_json::to_value(&m).unwrap();
        assert!(j.get("run").is_none());
        assert_eq!(j["provides"][1]["pattern"], json!(true));
        assert!(j["provides"][0].get("pattern").is_none());
        // what's refused
        let bad = |s: &str| check(&serde_json::from_str::<Manifest>(s).unwrap()).unwrap_err();
        assert!(bad(r#"{ "id": "Bad Id", "name": "x" }"#).contains("id"));
        assert!(bad(r#"{ "id": "w", "name": "x", "settings": [ { "key": "k", "label": "", "type": "slider" } ] }"#).contains("unknown type"));
        assert!(bad(r#"{ "id": "w", "name": "x", "settings": [ { "key": "k", "label": "", "type": "choice" } ] }"#).contains("no choices"));
        assert!(bad(r#"{ "id": "w", "name": "x", "settings": [ { "key": "n", "label": "", "type": "number", "min": 5, "max": 1 } ] }"#)
            .contains("min above its max"));
    }

    /// A setting's help, placeholder, min and max reach the studio; a choice without a default falls
    /// back to its first choice.
    #[test]
    fn setting_extras() {
        let m: Manifest = serde_json::from_str(r#"{ "id": "w", "name": "W", "settings": [
            { "key": "n", "label": "N", "type": "number", "default": 3, "min": 1, "max": 10.5, "help": "How many.", "placeholder": "3" },
            { "key": "units", "label": "Units", "type": "choice", "choices": ["c", "f"] },
            { "key": "city", "label": "City", "type": "text" } ] }"#).unwrap();
        check(&m).unwrap();
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["settings"][0], json!({ "key": "n", "label": "N", "type": "number", "default": 3, "min": 1.0, "max": 10.5,
                                              "help": "How many.", "placeholder": "3" }));
        assert_eq!(j["settings"][2], json!({ "key": "city", "label": "City", "type": "text" }));
        assert_eq!(m.settings[0].fallback(), Some(json!(3)));
        assert_eq!(m.settings[1].fallback(), Some(json!("c")));
        assert_eq!(m.settings[2].fallback(), None);
        // the catalog the API serves carries them
        let catalog = Catalog { manifests: vec![m], problems: Vec::new() };
        assert_eq!(catalog.json(|_| json!({}))["sources"][0]["settings"][0]["help"], json!("How many."));
        // a help that isn't text is a broken plugin.json, not a silent drop
        assert!(serde_json::from_str::<Manifest>(r#"{ "id": "w", "name": "W", "settings": [
            { "key": "k", "label": "", "type": "text", "help": 5 } ] }"#).is_err());
    }

    /// Plugin settings are stored beside `type`, `enabled` and `interval` in data.json, so only those
    /// keys are refused: the built-ins' own setting keys are free for a plugin (see data::Source).
    #[test]
    fn setting_keys() {
        let manifest = |key: &str| format!(r#"{{ "id": "w", "name": "W", "settings": [ {{ "key": "{key}", "label": "", "type": "text" }} ] }}"#);
        for key in ["type", "enabled", "interval", ""] {
            let e = check(&serde_json::from_str(&manifest(key)).unwrap()).unwrap_err();
            assert!(e.contains("can't have that key"), "{key}: {e}");
        }
        for key in ["hwinfo", "lhm", "lhm_url", "url", "token", "entities", "enabled_at", "offset", "set"] {
            check(&serde_json::from_str(&manifest(key)).unwrap()).unwrap_or_else(|e| panic!("{key}: {e}"));
        }
        // the studio's password flags, and one key twice
        for key in ["token_set", "city_set"] {
            let e = check(&serde_json::from_str(&manifest(key)).unwrap()).unwrap_err();
            assert!(e.contains("can't have that key") && e.contains("_set"), "{key}: {e}");
        }
        let twice: Manifest = serde_json::from_str(r#"{ "id": "w", "name": "W", "settings": [ { "key": "city", "label": "", "type": "text" },
            { "key": "n", "label": "", "type": "number" }, { "key": "city", "label": "", "type": "number" } ] }"#).unwrap();
        assert_eq!(check(&twice).unwrap_err(), "setting \"city\" is there twice: each setting needs a key of its own");
    }

    /// An interval is 1 s to a day: a larger one would overflow the player's waits.
    #[test]
    fn intervals() {
        let manifest = |interval: &str| format!(r#"{{ "id": "w", "name": "W", "interval": {interval} }}"#);
        for ok in [r#"{ "default": 86400, "min": 86400 }"#, r#"{ "default": 1, "min": 1 }"#, r#"{ "default": 2.5, "min": 1.5 }"#] {
            check(&serde_json::from_str(&manifest(ok)).unwrap()).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [r#"{ "default": 1e19, "min": 5 }"#, r#"{ "default": 60, "min": 1e300 }"#, r#"{ "default": 86401, "min": 5 }"#,
                    r#"{ "default": 60, "min": 0.5 }"#, r#"{ "default": 0, "min": 5 }"#] {
            let e = check(&serde_json::from_str(&manifest(bad)).unwrap()).unwrap_err();
            assert_eq!(e, "the interval's min and default should be numbers from 1 to 86400 (a day)", "{bad}");
        }
    }

    #[test]
    fn scanning_a_folder() {
        let dir = std::env::temp_dir().join(format!("wvplay-catalog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let make = |name: &str, body: &str| {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join("plugin.json"), body).unwrap();
        };
        make("good", r#"{ "id": "good", "name": "Good", "run": { "command": ["cmd", "/c", "echo"] } }"#);
        make("broken", "{ not json");
        make("norun", r#"{ "id": "norun", "name": "No run" }"#);
        make("clash", r#"{ "id": "pc", "name": "PC again", "run": { "command": ["x"] } }"#);
        make("mode", r#"{ "id": "mode", "name": "Mode", "run": { "command": ["x"], "mode": "push" } }"#);
        make("keys", r#"{ "id": "keys", "name": "Keys", "provides": [ { "key": "claude.week", "label": "" } ], "run": { "command": ["x"] } }"#);
        make("good2", r#"{ "id": "good", "name": "Good again", "run": { "command": ["x"] } }"#);
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        let found = scan(&dir);
        let ids: Vec<&str> = found.manifests.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["claude", "pc", "homeassistant", "time", "good"]);
        assert_eq!(found.get("good").unwrap().run.as_ref().unwrap().dir, dir.join("good"));
        assert_eq!(found.problems.len(), 6, "{:?}", found.problems);
        assert!(found.problems.iter().any(|p| p.contains("keys") && p.contains("isn't under")));
        assert!(found.problems.iter().any(|p| p.contains("broken") && p.contains("isn't valid")));
        let problem = |folder: &str| found.problems.iter().find(|p| p.starts_with(&dir.join(folder).display().to_string())).cloned();
        assert_eq!(problem("clash").unwrap(), format!("{}: the id \"pc\" is the built-in This PC source's: choose another", dir.join("clash").display()));
        assert_eq!(problem("good2").unwrap(), format!("{}: the plugin in {} already has the id \"good\"", dir.join("good2").display(), dir.join("good").display()));
        // a missing folder is just no plugins
        let none = scan(&dir.join("nowhere"));
        assert_eq!(none.manifests.len(), 4);
        assert!(none.problems.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A plugin can't take a built-in's id, nor the first part of the keys a built-in sets, whose
    /// values the two would overwrite.
    #[test]
    fn taken_ids() {
        let empty = Catalog { manifests: builtins(), problems: Vec::new() };
        for id in ["claude", "pc", "homeassistant", "time"] {
            let why = taken(id, &empty).unwrap_or_else(|| panic!("{id} wasn't refused"));
            assert!(why.contains("is the built-in") && why.ends_with("source's: choose another"), "{why}");
        }
        assert_eq!(taken("ha", &empty).unwrap(), "the id \"ha\" is taken: the built-in Home Assistant source sets keys starting with \"ha.\"");
        assert_eq!(taken("hwinfo", &empty).unwrap(), "the id \"hwinfo\" is taken: the built-in This PC source sets keys starting with \"hwinfo.\"");
        assert!(taken("lhm", &empty).unwrap().contains("This PC"));
        for id in ["weather", "disk", "hw", "claudes", "pc2", "home"] {
            assert_eq!(taken(id, &empty), None, "{id}");
        }
        assert!(is_builtin("pc") && is_builtin("time") && !is_builtin("ha") && !is_builtin("weather"));
    }
}
