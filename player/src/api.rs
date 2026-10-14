//! The player's local API (docs/data.md, docs/sources.md), for the studio, which proxies it to the
//! browser: the values, filling in text, trying a source out, and the catalog. On 127.0.0.1 only, and
//! only answering requests addressed to 127.0.0.1 or localhost, so a web page can't reach it through a
//! DNS name pointed here.

use std::io::Read;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value as Json, json};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::data::{self, Hub, Source, iso, store};
use crate::player::log;
use crate::sources::Polled;

/// Serves the API on the port in data.json, moving when it changes. Runs for good.
pub fn serve(hub: Arc<Hub>) {
    let mut failed = None;
    loop {
        let port = hub.port();
        match Server::http(("127.0.0.1", port)) {
            Ok(server) => {
                failed = None;
                log(format_args!("data: API on http://127.0.0.1:{port}"));
                let server = Arc::new(server);
                *hub.server.lock().unwrap() = Some(server.clone());
                for request in server.incoming_requests() {
                    handle(&hub, request);
                }
            }
            Err(e) => {
                // most likely another player still has the port: keep trying, but say so once
                if failed != Some(port) {
                    log(format_args!("data: no API on port {port} yet: {e}"));
                    failed = Some(port);
                }
                sleep(Duration::from_secs(5));
            }
        }
    }
}

fn local(request: &Request) -> bool {
    local_host(request.headers().iter().find(|h| h.field.equiv("Host")).map(|h| h.value.as_str()))
}

/// Whether a Host header names this machine by 127.0.0.1 or localhost, with or without a port.
fn local_host(host: Option<&str>) -> bool {
    let Some(host) = host.map(|h| h.trim().to_ascii_lowercase()) else { return false };
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host.as_str(),
    };
    name == "127.0.0.1" || name == "localhost"
}

fn respond(request: Request, status: u16, body: Json) {
    let header = Header::from_bytes("Content-Type", "application/json").unwrap();
    let _ = request.respond(Response::from_string(body.to_string()).with_status_code(status).with_header(header));
}

fn handle(hub: &Arc<Hub>, mut request: Request) {
    if !local(&request) {
        return respond(request, 403, json!({ "error": "only for 127.0.0.1 or localhost" }));
    }
    let mut body = String::new();
    let _ = request.as_reader().take(1 << 20).read_to_string(&mut body);
    let body: Json = serde_json::from_str(&body).unwrap_or(Json::Null);
    let path = request.url().split('?').next().unwrap_or("").to_string();
    match (request.method(), path.as_str()) {
        (Method::Get, "/values") => respond(request, 200, values()),
        (Method::Get, "/catalog") => respond(request, 200, hub.catalog()),
        (Method::Post, "/catalog/rescan") => {
            let found = hub.rescan();
            let plugins = found.manifests.iter().filter(|m| m.run.is_some()).count();
            respond(request, 200, json!({ "plugins": plugins, "problems": found.problems, "folder": hub.plugins.to_string_lossy() }))
        }
        (Method::Post, "/resolve") => match body["texts"].as_array() {
            Some(texts) => {
                let filled: Vec<String> = texts.iter().map(|t| data::resolve(t.as_str().unwrap_or(""))).collect();
                respond(request, 200, json!({ "texts": filled }))
            }
            None => respond(request, 400, json!({ "error": "expected {\"texts\": [...]}" })),
        },
        (Method::Post, "/test") => match serde_json::from_value::<Source>(body["source"].clone()) {
            // a poll can take a while: answer from its own thread, so other requests don't wait
            Ok(source) => {
                let hub = hub.clone();
                std::thread::spawn(move || {
                    let answer = match hub.test(source) {
                        Ok(Polled { values, notes }) => {
                            let now = Utc::now();
                            let values: serde_json::Map<String, Json> = values.iter().map(|(k, v)| (k.clone(), v.json(now))).collect();
                            json!({ "values": values, "notes": notes })
                        }
                        Err(e) => json!({ "error": e }),
                    };
                    respond(request, 200, answer);
                });
            }
            Err(_) => respond(request, 400, json!({ "error": "expected {\"source\": {...}}" })),
        },
        _ => respond(request, 404, json!({ "error": "not found" })),
    }
}

/// `{"<key>": {"value", "text", "at", "stale", "source"}}`: `text` is the default form, as `{key}` shows.
fn values() -> Json {
    let now = Utc::now();
    let values: serde_json::Map<String, Json> = store()
        .list(now)
        .into_iter()
        .map(|(key, value, at, stale, source)| {
            let entry = json!({ "value": value.json(now), "text": value.show("", now), "at": iso(at), "stale": stale, "source": source });
            (key, entry)
        })
        .collect();
    Json::Object(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_check() {
        for ok in ["127.0.0.1", "localhost", "127.0.0.1:8139", "localhost:8139", "LocalHost:80", "127.0.0.1:"] {
            assert!(local_host(Some(ok)), "{ok}");
        }
        for bad in ["evil.example", "evil.example:8139", "127.0.0.1.evil.example", "localhost.evil:8139",
                    "127.0.0.1:x", "0.0.0.0:8139", "[::1]:8139", ""] {
            assert!(!local_host(Some(bad)), "{bad}");
        }
        assert!(!local_host(None));
    }

    /// A scratch playlist folder with a data.json, a plugins folder beside it, and a player API on a
    /// free port.
    #[test]
    fn catalog_and_rescan() {
        let root = std::env::temp_dir().join(format!("wvplay-api-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let plugins = root.join("plugins");
        std::fs::create_dir_all(root.join("playlist")).unwrap();
        std::fs::create_dir_all(&plugins).unwrap();
        let plugin = |id: &str, body: &str| {
            std::fs::create_dir_all(plugins.join(id)).unwrap();
            std::fs::write(plugins.join(id).join("p.cmd"), body).unwrap();
            let manifest = json!({ "id": id, "name": id, "settings": [ { "key": "city", "label": "City", "type": "text", "default": "Oslo" },
                                                                        { "key": "key", "label": "Key", "type": "password" } ],
                                   "provides": [ { "key": format!("{id}.n"), "label": "N", "kind": "number" } ],
                                   "run": { "command": ["cmd", "/c", ".\\p.cmd"], "mode": "poll" } });
            std::fs::write(plugins.join(id).join("plugin.json"), manifest.to_string()).unwrap();
        };
        plugin("one", "@echo {\"values\": {\"one.n\": 1}}
");
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let playlist = root.join("playlist").join("playlist.json");
        std::fs::write(data::file_for(&playlist), json!({ "api_port": port,
            "sources": [ { "type": "one", "enabled": true, "interval": 5, "key": "s3cret" } ] }).to_string()).unwrap();
        let hub = data::start(&playlist, true);
        assert_eq!(hub.plugins, plugins);
        hub.wait(Duration::from_secs(20));
        let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().into();
        let url = |path: &str| format!("http://127.0.0.1:{port}{path}");
        let get = |path: &str| -> Json {
            for _ in 0..50 {
                if let Ok(mut r) = agent.get(url(path)).call() {
                    return serde_json::from_str(&r.body_mut().read_to_string().unwrap()).unwrap();
                }
                sleep(Duration::from_millis(100));
            }
            panic!("no API on {port}");
        };
        let post = |path: &str, body: Json| -> Json {
            let mut r = agent.post(url(path)).send(body.to_string()).unwrap();
            serde_json::from_str(&r.body_mut().read_to_string().unwrap()).unwrap()
        };
        let catalog = get("/catalog");
        let sources = catalog["sources"].as_array().unwrap();
        let ids: Vec<&str> = sources.iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["claude", "pc", "homeassistant", "time", "one"]);
        assert!(sources.iter().all(|s| s.get("run").is_none()));
        let one = &sources[4];
        assert_eq!(one["status"]["enabled"], json!(true));
        assert_eq!(one["status"]["ok"], json!(true), "{}", one["status"]);
        assert!(one["status"]["updated_at"].is_string());
        assert_eq!(one["plugin"], json!(true));
        assert_eq!(sources[0]["status"], json!({ "enabled": false, "ok": false, "error": null, "notes": [], "updated_at": null }));
        assert_eq!(sources[3]["status"]["enabled"], json!(true));
        assert_eq!(get("/values")["one.n"]["value"], json!(1.0));
        // a new plugin shows up on rescan; a broken one is reported
        plugin("two", "@echo {\"values\": {\"two.n\": 2, \"one.n\": 9}}
");
        std::fs::create_dir_all(plugins.join("bad")).unwrap();
        std::fs::write(plugins.join("bad").join("plugin.json"), "{").unwrap();
        let scanned = post("/catalog/rescan", Json::Null);
        assert_eq!(scanned["plugins"], json!(2));
        assert_eq!(scanned["problems"].as_array().unwrap().len(), 1);
        let catalog = get("/catalog");
        let two = &catalog["sources"][5];
        assert_eq!(two["id"], json!("two"));
        assert_eq!(two["status"]["enabled"], json!(false));
        // /test runs a plugin once with the given settings, taking the saved password when none is given
        let tested = post("/test", json!({ "source": { "type": "two", "city": "Rome" } }));
        assert_eq!(tested, json!({ "values": { "two.n": 2.0 }, "notes": ["ignored one.n isn't under two."] }));
        plugin("one", "@set /p in=
@echo {\"values\": {\"one.in\": \"%in:\"=%\"}}
");
        let tested = post("/test", json!({ "source": { "type": "one", "key": "" } }));
        let shown = tested["values"]["one.in"].as_str().unwrap_or_default();
        assert!(shown.contains("s3cret") && shown.contains("Oslo"), "{tested}");
        let tested = post("/test", json!({ "source": { "type": "nope" } }));
        assert_eq!(tested["error"], json!("unknown source type \"nope\""));
        let _ = std::fs::remove_dir_all(&root);
    }
}
