//! Local web dashboard (tiny_http): GET / and /api/state, POST controls.

use rds_fm::app::{App, Mode};
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Request, Response, Server};

const INDEX: &str = include_str!("../web/index.html");

type Resp = Response<Cursor<Vec<u8>>>;

fn with_type(r: Resp, ct: &str) -> Resp {
    r.with_header(Header::from_bytes(&b"Content-Type"[..], ct.as_bytes()).unwrap())
}

fn json(v: serde_json::Value) -> Resp {
    with_type(Response::from_string(v.to_string()), "application/json; charset=utf-8")
}

fn bad(msg: &str) -> Resp {
    json(serde_json::json!({ "ok": false, "error": msg })).with_status_code(400)
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.replace("%2F", "/").replace('+', " "))
    })
}

fn handle(app: &Arc<Mutex<App>>, req: &Request) -> Resp {
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let ok = || json(serde_json::json!({ "ok": true }));
    match (req.method(), path) {
        (Method::Get, "/") | (Method::Get, "/index.html") => {
            with_type(Response::from_string(INDEX), "text/html; charset=utf-8")
        }
        (Method::Get, "/api/state") => json(app.lock().unwrap().to_json()),
        (Method::Post, "/api/scan") => {
            let mut a = app.lock().unwrap();
            if a.mode != Mode::Scan {
                a.mode = Mode::Scan;
                a.push_log("Scanning the whole band");
            }
            ok()
        }
        (Method::Post, "/api/tune") => {
            let Some(f) = query_param(query, "f").and_then(|s| s.parse::<f64>().ok()) else {
                return bad("missing f (MHz)");
            };
            if !(0.1..=6000.0).contains(&f) {
                return bad("frequency must be given in MHz");
            }
            let mut a = app.lock().unwrap();
            a.listen((f * 1e6 / 1e3).round() * 1e3);
            a.push_log(format!("Listening {:.3} MHz", f));
            ok()
        }
        (Method::Post, "/api/stop") => {
            app.lock().unwrap().mode = Mode::Idle;
            ok()
        }
        (Method::Post, "/api/record") => {
            let on = query_param(query, "on").map(|v| v == "1" || v == "true").unwrap_or(true);
            let mut a = app.lock().unwrap();
            if on && !matches!(a.mode, Mode::Listen { .. }) {
                return bad("recording needs listen mode (POST /api/tune first)");
            }
            a.rec_request = Some(on);
            ok()
        }
        (Method::Post, "/api/volume") => {
            let Some(v) = query_param(query, "v").and_then(|s| s.parse::<f32>().ok()) else {
                return bad("missing v (0–1.5)");
            };
            let mut a = app.lock().unwrap();
            a.audio.volume = v.clamp(0.0, 1.5);
            a.settings_dirty = true;
            ok()
        }
        (Method::Post, "/api/gain") => {
            let Some(g) = query_param(query, "g").and_then(|s| s.parse::<f64>().ok()) else {
                return bad("missing g (dB)");
            };
            if !(0.0..=76.0).contains(&g) {
                return bad("gain must be 0–76 dB");
            }
            let mut a = app.lock().unwrap();
            a.gain = g;
            a.push_log(format!("Gain set to {g:.0} dB"));
            ok()
        }
        (Method::Post, "/api/antenna") => {
            let Some(ant) = query_param(query, "a").filter(|a| a == "TX/RX" || a == "RX2") else {
                return bad("antenna must be TX/RX or RX2");
            };
            let mut a = app.lock().unwrap();
            a.antenna = ant.clone();
            a.push_log(format!("Antenna port set to {ant}"));
            ok()
        }
        (Method::Post, "/api/clear") => {
            let mut a = app.lock().unwrap();
            a.stations.clear();
            a.push_log("Station list cleared");
            ok()
        }
        _ => with_type(Response::from_string("not found"), "text/plain").with_status_code(404),
    }
}

pub fn serve(app: Arc<Mutex<App>>, port: u16) -> Result<(), String> {
    let server = Server::http(("127.0.0.1", port)).map_err(|e| format!("cannot open port {port}: {e}"))?;
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let resp = handle(&app, &req);
            let _ = req.respond(resp);
        }
    });
    Ok(())
}
