//! Internet radio streaming: live MP3 encoding, a built-in Icecast-style HTTP server
//! (ICY metadata = RDS "now playing"), and push to an external Icecast server.

use crate::audio::{Frame, Resampler};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type Chunk = Arc<Vec<u8>>;
pub type Log = Arc<dyn Fn(String) + Send + Sync>;

// ---------------------------------------------------------------- encoder

/// Real-time MP3 encoder (LAME, CBR) for 48 kHz stereo input.
pub struct LiveMp3 {
    enc: mp3lame_encoder::Encoder,
    rs: Option<Resampler>,
    tmp: Vec<Frame>,
    l: Vec<i16>,
    r: Vec<i16>,
    out: Vec<u8>,
}

impl LiveMp3 {
    pub fn new(kbps: u32, out_rate: u32) -> Result<Self, String> {
        use mp3lame_encoder::{Bitrate, Builder, Quality};
        let br = match kbps {
            0..=64 => Bitrate::Kbps64,
            65..=96 => Bitrate::Kbps96,
            97..=128 => Bitrate::Kbps128,
            129..=160 => Bitrate::Kbps160,
            161..=192 => Bitrate::Kbps192,
            193..=256 => Bitrate::Kbps256,
            _ => Bitrate::Kbps320,
        };
        let mut b = Builder::new().ok_or("LAME init failed")?;
        b.set_num_channels(2).map_err(|e| format!("{e:?}"))?;
        b.set_sample_rate(out_rate).map_err(|e| format!("{e:?}"))?;
        b.set_brate(br).map_err(|e| format!("{e:?}"))?;
        b.set_quality(Quality::Good).map_err(|e| format!("{e:?}"))?;
        let enc = b.build().map_err(|e| format!("{e:?}"))?;
        let rs = (out_rate != 48_000).then(|| Resampler::new(48_000.0, out_rate as f64));
        Ok(Self { enc, rs, tmp: Vec::new(), l: Vec::new(), r: Vec::new(), out: Vec::new() })
    }

    /// Encode 48 kHz frames; returns MP3 bytes produced so far (may be empty).
    pub fn encode(&mut self, frames: &[Frame]) -> Vec<u8> {
        let src: &[Frame] = match &mut self.rs {
            Some(rs) => {
                self.tmp.clear();
                rs.process(frames, &mut self.tmp);
                &self.tmp
            }
            None => frames,
        };
        self.l.clear();
        self.r.clear();
        for f in src {
            self.l.push((f[0].clamp(-1.0, 1.0) * 32767.0) as i16);
            self.r.push((f[1].clamp(-1.0, 1.0) * 32767.0) as i16);
        }
        self.out.clear();
        self.out.reserve(self.l.len() * 5 / 4 + 7200);
        let _ = self.enc.encode_to_vec(mp3lame_encoder::DualPcm { left: &self.l, right: &self.r }, &mut self.out);
        std::mem::take(&mut self.out)
    }
}

// ---------------------------------------------------------------- hub

#[derive(Clone, Debug, Default)]
pub struct MountInfo {
    pub mount: String,
    pub name: String,
    pub title: String,
    pub listeners: usize,
    pub bytes: u64,
    pub kbps: u32,
}

struct Mount {
    info: MountInfo,
    subs: Vec<SyncSender<Chunk>>,
    burst: VecDeque<Chunk>,
    burst_bytes: usize,
    title: Arc<Mutex<String>>,
}

/// Fan-out of encoded audio to any number of listeners per mount.
#[derive(Default)]
pub struct Hub {
    mounts: Mutex<HashMap<String, Mount>>,
}

pub fn mount_name(freq_hz: f64) -> String {
    if freq_hz < 30e6 {
        format!("am{:.0}", freq_hz / 1e3)
    } else {
        format!("fm{:.0}", freq_hz / 1e4) // fm9700 = 97.00 MHz
    }
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn open(&self, mount: &str, name: &str, kbps: u32) {
        let mut m = self.mounts.lock().unwrap();
        let e = m.entry(mount.to_string()).or_insert_with(|| Mount {
            info: MountInfo { mount: mount.to_string(), ..Default::default() },
            subs: Vec::new(),
            burst: VecDeque::new(),
            burst_bytes: 0,
            title: Arc::new(Mutex::new(String::new())),
        });
        e.info.name = name.to_string();
        e.info.kbps = kbps;
    }

    pub fn close(&self, mount: &str) {
        self.mounts.lock().unwrap().remove(mount);
    }

    pub fn publish(&self, mount: &str, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        let chunk: Chunk = Arc::new(data);
        let mut m = self.mounts.lock().unwrap();
        let Some(mt) = m.get_mut(mount) else { return };
        mt.info.bytes += chunk.len() as u64;
        mt.burst_bytes += chunk.len();
        mt.burst.push_back(chunk.clone());
        // keep ~2 s of audio so a new listener starts without a gap
        let keep = (mt.info.kbps as usize * 1000 / 8) * 2;
        while mt.burst_bytes > keep && mt.burst.len() > 1 {
            mt.burst_bytes -= mt.burst.pop_front().unwrap().len();
        }
        mt.subs.retain(|s| !matches!(s.try_send(chunk.clone()), Err(TrySendError::Disconnected(_))));
        mt.info.listeners = mt.subs.len();
    }

    pub fn set_title(&self, mount: &str, title: &str) {
        if let Some(mt) = self.mounts.lock().unwrap().get_mut(mount) {
            if mt.info.title != title {
                mt.info.title = title.to_string();
                *mt.title.lock().unwrap() = title.to_string();
            }
        }
    }

    fn subscribe(&self, mount: &str) -> Option<(Receiver<Chunk>, Vec<Chunk>, Arc<Mutex<String>>, MountInfo)> {
        let mut m = self.mounts.lock().unwrap();
        let mt = m.get_mut(mount)?;
        let (tx, rx) = sync_channel(256);
        mt.subs.push(tx);
        mt.info.listeners = mt.subs.len();
        Some((rx, mt.burst.iter().cloned().collect(), mt.title.clone(), mt.info.clone()))
    }

    pub fn list(&self) -> Vec<MountInfo> {
        let mut v: Vec<MountInfo> = self.mounts.lock().unwrap().values().map(|m| m.info.clone()).collect();
        v.sort_by(|a, b| a.mount.cmp(&b.mount));
        v
    }
}

// ---------------------------------------------------------------- HTTP server

/// Stream body: chunks from the hub, with ICY metadata blocks when requested.
struct StreamBody {
    rx: Receiver<Chunk>,
    queue: VecDeque<Chunk>,
    cur: Chunk,
    pos: usize,
    icy: Option<(usize, usize, Arc<Mutex<String>>)>, // (metaint, bytes until next meta, title)
    meta: Vec<u8>,
    meta_pos: usize,
}

const META_INT: usize = 16_000;

impl Read for StreamBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.meta_pos < self.meta.len() {
            let n = (self.meta.len() - self.meta_pos).min(buf.len());
            buf[..n].copy_from_slice(&self.meta[self.meta_pos..self.meta_pos + n]);
            self.meta_pos += n;
            return Ok(n);
        }
        while self.pos >= self.cur.len() {
            self.cur = match self.queue.pop_front() {
                Some(c) => c,
                None => match self.rx.recv_timeout(Duration::from_secs(10)) {
                    Ok(c) => c,
                    Err(_) => return Ok(0), // mount closed or stalled: end the response
                },
            };
            self.pos = 0;
        }
        let mut n = (self.cur.len() - self.pos).min(buf.len());
        if let Some((metaint, until, title)) = &mut self.icy {
            if *until == 0 {
                let t = title.lock().unwrap().replace('\'', "’");
                let s = format!("StreamTitle='{t}';");
                let blocks = s.len().div_ceil(16).min(255);
                let mut m = vec![blocks as u8];
                m.extend_from_slice(&s.as_bytes()[..s.len().min(blocks * 16)]);
                m.resize(1 + blocks * 16, 0);
                self.meta = m;
                self.meta_pos = 0;
                *until = *metaint;
                return self.read(buf);
            }
            n = n.min(*until);
            *until -= n;
        }
        buf[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn index_page(hub: &Hub) -> String {
    let mut rows = String::new();
    for m in hub.list() {
        rows += &format!(
            "<div class=card><h2>{}</h2><p class=now>{}</p><audio controls preload=none src=\"/{}\"></audio>\
             <p class=meta>/{} · {} kbps MP3 · {} listening</p></div>",
            html_escape(&m.name),
            html_escape(&m.title),
            m.mount,
            m.mount,
            m.kbps,
            m.listeners
        );
    }
    if rows.is_empty() {
        rows = "<p>No station is on air right now.</p>".into();
    }
    format!(
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <meta http-equiv=refresh content=20><title>RDS FM streams</title><style>\
         :root{{--bg:#f5f6f8;--card:#fff;--ink:#1b1f24;--muted:#5d6673}}\
         @media (prefers-color-scheme:dark){{:root{{--bg:#111418;--card:#1a1e24;--ink:#e6e9ee;--muted:#9aa4b2}}}}\
         body{{margin:0;padding:16px;background:var(--bg);color:var(--ink);font:15px system-ui,'Segoe UI','Leelawadee UI',sans-serif}}\
         .card{{background:var(--card);border-radius:10px;padding:14px 16px;margin:0 0 12px;max-width:640px}}\
         h1{{font-size:20px}}h2{{margin:0 0 4px;font-size:18px}}.now{{margin:0 0 8px}}.meta{{color:var(--muted);font-size:12px;margin:6px 0 0}}\
         audio{{width:100%}}</style><h1>📻 RDS FM streams</h1>{rows}</html>"
    )
}

fn respond_simple(s: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.0 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
}

/// One client connection. Streams are sent Icecast-style: an HTTP/1.0 response with
/// no length, then raw MP3 until the listener goes away (works with browsers, VLC,
/// Winamp, foobar2000 and other internet-radio players).
fn handle(hub: &Hub, mut s: TcpStream) {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut req = Vec::new();
    let mut buf = [0u8; 1024];
    while !req.windows(4).any(|w| w == b"\r\n\r\n") && req.len() < 16 * 1024 {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => req.extend_from_slice(&buf[..n]),
        }
    }
    let text = String::from_utf8_lossy(&req);
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let (method, url) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    if method != "GET" && method != "HEAD" {
        respond_simple(&mut s, "405 Method Not Allowed", "text/plain", b"GET only");
        return;
    }
    let want_icy = lines.any(|l| {
        let (k, v) = l.split_once(':').unwrap_or(("", ""));
        k.trim().eq_ignore_ascii_case("icy-metadata") && v.trim() == "1"
    });
    let path = url.split('?').next().unwrap_or("/").trim_start_matches('/').trim_end_matches(".mp3").to_string();
    if path.is_empty() || path == "index.html" {
        respond_simple(&mut s, "200 OK", "text/html; charset=utf-8", index_page(hub).as_bytes());
        return;
    }
    if path == "status.json" {
        let v: Vec<serde_json::Value> = hub
            .list()
            .into_iter()
            .map(|m| serde_json::json!({"mount": m.mount, "name": m.name, "title": m.title, "listeners": m.listeners, "kbps": m.kbps}))
            .collect();
        respond_simple(&mut s, "200 OK", "application/json", serde_json::Value::Array(v).to_string().as_bytes());
        return;
    }
    let Some((rx, burst, title, info)) = hub.subscribe(&path) else {
        respond_simple(&mut s, "404 Not Found", "text/plain", b"no such stream");
        return;
    };
    let mut head = format!(
        "HTTP/1.0 200 OK\r\nContent-Type: audio/mpeg\r\nCache-Control: no-cache, no-store\r\n\
         icy-name: {}\r\nicy-br: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n",
        info.name.replace(['\r', '\n'], " "),
        info.kbps
    );
    if want_icy {
        head += &format!("icy-metaint: {META_INT}\r\n");
    }
    head += "\r\n";
    if s.write_all(head.as_bytes()).is_err() || method == "HEAD" {
        return;
    }
    let mut body = StreamBody {
        rx,
        queue: burst.into(),
        cur: Arc::new(Vec::new()),
        pos: 0,
        icy: want_icy.then(|| (META_INT, META_INT, title)),
        meta: Vec::new(),
        meta_pos: 0,
    };
    s.set_write_timeout(Some(Duration::from_secs(15))).ok();
    let mut out = vec![0u8; 16 * 1024];
    loop {
        match body.read(&mut out) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if s.write_all(&out[..n]).is_err() {
                    break;
                }
            }
        }
    }
}

/// Running HTTP stream server; dropping it stops listening.
pub struct Server {
    stop: Arc<AtomicBool>,
    pub bind: String,
    pub port: u16,
}

impl Server {
    pub fn start(hub: Arc<Hub>, bind: &str) -> Result<Self, String> {
        let listener = std::net::TcpListener::bind(bind).map_err(|e| format!("cannot listen on {bind}: {e}"))?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        std::thread::spawn(move || {
            while !st.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((sock, _)) => {
                        let _ = sock.set_nonblocking(false);
                        let hub = hub.clone();
                        std::thread::spawn(move || handle(&hub, sock));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        });
        Ok(Self { stop, bind: bind.to_string(), port })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- Icecast source

#[derive(Clone, Debug, PartialEq)]
pub struct IcecastCfg {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
}

impl Default for IcecastCfg {
    fn default() -> Self {
        Self { enabled: false, host: "localhost".into(), port: 8000, user: "source".into(), password: String::new() }
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn icecast_metadata(cfg: &IcecastCfg, mount: &str, title: &str) {
    let auth = base64(format!("{}:{}", cfg.user, cfg.password).as_bytes());
    if let Ok(mut s) = TcpStream::connect((cfg.host.as_str(), cfg.port)) {
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        let req = format!(
            "GET /admin/metadata?mount=/{mount}&mode=updinfo&song={} HTTP/1.0\r\nHost: {}\r\nAuthorization: Basic {auth}\r\nUser-Agent: rds-fm\r\n\r\n",
            url_encode(title),
            cfg.host
        );
        let _ = s.write_all(req.as_bytes());
        let mut sink = [0u8; 512];
        let _ = s.read(&mut sink);
    }
}

/// Push one mount to an Icecast 2.4+ server (HTTP PUT source protocol) until `stop`.
pub fn spawn_icecast(hub: Arc<Hub>, cfg: IcecastCfg, mount: String, stop: Arc<AtomicBool>, log: Log) {
    std::thread::spawn(move || {
        let auth = base64(format!("{}:{}", cfg.user, cfg.password).as_bytes());
        let mut announced = false;
        while !stop.load(Ordering::Relaxed) {
            let res = (|| -> Result<(), String> {
                let Some((rx, _, title, info)) = hub.subscribe(&mount) else {
                    return Err("mount not open".into());
                };
                let mut s = TcpStream::connect((cfg.host.as_str(), cfg.port)).map_err(|e| e.to_string())?;
                s.set_read_timeout(Some(Duration::from_secs(10))).ok();
                s.set_write_timeout(Some(Duration::from_secs(10))).ok();
                let req = format!(
                    "PUT /{mount} HTTP/1.1\r\nHost: {}:{}\r\nAuthorization: Basic {auth}\r\nUser-Agent: rds-fm\r\n\
                     Content-Type: audio/mpeg\r\nIce-Public: 0\r\nIce-Name: {}\r\nIce-Bitrate: {}\r\nExpect: 100-continue\r\n\r\n",
                    cfg.host, cfg.port, info.name, info.kbps
                );
                s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
                let mut resp = [0u8; 1024];
                let n = s.read(&mut resp).map_err(|e| e.to_string())?;
                let head = String::from_utf8_lossy(&resp[..n]).to_string();
                let status = head.lines().next().unwrap_or("").to_string();
                if !(status.contains(" 100") || status.contains(" 200")) {
                    return Err(format!("server refused: {status}"));
                }
                if !announced {
                    log(format!("Icecast: streaming /{mount} to {}:{}", cfg.host, cfg.port));
                    announced = true;
                }
                let mut last_title = String::new();
                while !stop.load(Ordering::Relaxed) {
                    let chunk = rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "no audio".to_string())?;
                    s.write_all(&chunk).map_err(|e| e.to_string())?;
                    let t = title.lock().unwrap().clone();
                    if t != last_title && !t.is_empty() {
                        icecast_metadata(&cfg, &mount, &t);
                        last_title = t;
                    }
                }
                Ok(())
            })();
            if let Err(e) = res {
                if !stop.load(Ordering::Relaxed) {
                    log(format!("Icecast /{mount}: {e} — retrying in 10 s"));
                    announced = false;
                    for _ in 0..100 {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_and_urlencode() {
        assert_eq!(base64(b"source:hackme"), "c291cmNlOmhhY2ttZQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(url_encode("NBT 97.0 - ข่าว"), "NBT%2097.0%20-%20%E0%B8%82%E0%B9%88%E0%B8%B2%E0%B8%A7");
    }

    #[test]
    fn http_stream_with_icy_metadata() {
        let hub = Hub::new();
        hub.open("fm9700", "NBT 97.0", 128);
        hub.set_title("fm9700", "RADIO THAILAND");
        let server = Server::start(hub.clone(), "127.0.0.1:0").unwrap();
        let port = server.port;
        // feed audio
        let h2 = hub.clone();
        let feeder = std::thread::spawn(move || {
            let mut enc = LiveMp3::new(128, 48_000).unwrap();
            for k in 0..60 {
                let f: Vec<Frame> = (0..4800).map(|i| {
                    let v = (0.3 * (std::f64::consts::TAU * 440.0 * (k * 4800 + i) as f64 / 48_000.0).sin()) as f32;
                    [v, v]
                }).collect();
                h2.publish("fm9700", enc.encode(&f));
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        std::thread::sleep(Duration::from_millis(200));
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(b"GET /fm9700 HTTP/1.0\r\nIcy-MetaData: 1\r\n\r\n").unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 8192];
        while got.len() < 40_000 {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
            }
        }
        feeder.join().unwrap();
        let text = String::from_utf8_lossy(&got);
        assert!(text.starts_with("HTTP/1."), "{}", &text[..40.min(text.len())]);
        let head = &text[..text.find("\r\n\r\n").unwrap_or(300).min(text.len())];
        assert!(head.contains("audio/mpeg") && head.to_ascii_lowercase().contains("icy-metaint: 16000"), "{head}");
        assert!(text.contains("StreamTitle='RADIO THAILAND';"), "no ICY title in {} bytes", got.len());
        // MP3 frame sync somewhere in the body
        assert!(got.windows(2).any(|w| w[0] == 0xFF && w[1] & 0xE0 == 0xE0));
        assert_eq!(hub.list()[0].mount, "fm9700");
        drop(server);
    }
}
