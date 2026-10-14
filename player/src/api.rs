//! The player's local API (docs/data.md), for the studio, which proxies it to the browser: the values,
//! filling in text, and trying a source out. On 127.0.0.1 only, and only answering requests addressed to
//! 127.0.0.1 or localhost, so a web page can't reach it through a DNS name pointed here.

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
}
