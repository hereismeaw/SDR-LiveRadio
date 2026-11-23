//! Shared application state (worker thread <-> GUI / web) and persisted settings.

use crate::broadcast::{Hub, IcecastCfg};
use crate::demod::StereoMode;
use crate::device::DeviceInfo;
use crate::enhance::EnhanceSettings;
use crate::record::{Format, RecordSettings};
use crate::station::Station;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// A saved frequency (memory channel).
#[derive(Clone, Debug, PartialEq)]
pub struct Preset {
    pub name: String,
    pub freq_hz: f64,
    pub notes: String,
}

/// One line of the reception logbook.
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub time: String,
    pub freq_hz: f64,
    pub kind: String,
    pub text: String,
}

/// A station to stream to the internet.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamCfg {
    pub freq_hz: f64,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamSettings {
    /// Built-in HTTP/Icecast-style server.
    pub server: bool,
    pub bind: String,
    pub kbps: u32,
    /// Also stream whatever is being listened to.
    pub stream_main: bool,
    pub icecast: IcecastCfg,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self { server: false, bind: "0.0.0.0:8000".into(), kbps: 128, stream_main: true, icecast: IcecastCfg::default() }
    }
}

/// Status of one stream in the running session.
#[derive(Clone, Debug)]
pub struct LiveStream {
    pub freq_hz: f64,
    pub mount: String,
    pub on_air: bool,
    pub note: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Demod {
    /// Wideband FM (broadcast) with stereo + RDS.
    Wfm,
    /// AM envelope (MW, SW, airband).
    Am,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Band {
    Fm,
    Mw,
    Sw,
    Air,
}

impl Band {
    pub const ALL: [Band; 4] = [Band::Fm, Band::Mw, Band::Sw, Band::Air];
    pub fn name(self) -> &'static str {
        match self {
            Band::Fm => "FM",
            Band::Mw => "MW (AM)",
            Band::Sw => "SW (AM)",
            Band::Air => "Air (AM)",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Band::Fm => "fm",
            Band::Mw => "mw",
            Band::Sw => "sw",
            Band::Air => "air",
        }
    }
    pub fn from_key(s: &str) -> Option<Band> {
        Self::ALL.into_iter().find(|b| b.key() == s)
    }
    /// Hz
    pub fn range(self) -> (f64, f64) {
        match self {
            Band::Fm => (87.5e6, 108.0e6),
            Band::Mw => (531e3, 1_611e3),
            Band::Sw => (2.3e6, 26.1e6),
            Band::Air => (118e6, 137e6),
        }
    }
    /// Tuning step, Hz (Thailand: MW 9 kHz raster, SW 5 kHz, airband 25 kHz).
    pub fn step(self) -> f64 {
        match self {
            Band::Fm => 50e3,
            Band::Mw => 9e3,
            Band::Sw => 5e3,
            Band::Air => 25e3,
        }
    }
    pub fn demod(self) -> Demod {
        match self {
            Band::Fm => Demod::Wfm,
            _ => Demod::Am,
        }
    }
    pub fn default_freq(self) -> f64 {
        match self {
            Band::Fm => 97.0e6,
            Band::Mw => 891e3,
            Band::Sw => 9.39e6,
            Band::Air => 118.1e6,
        }
    }
    pub fn of_freq(hz: f64) -> Band {
        Self::ALL.into_iter().find(|b| hz >= b.range().0 - 1.0 && hz <= b.range().1 + 1.0).unwrap_or(Band::Fm)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Idle,
    /// Scan the FM band and decode RDS of every station.
    Scan,
    /// Listen to one frequency (audio, and RDS on FM).
    Listen { hz: u64, demod: Demod },
}

#[derive(Clone, Debug)]
pub struct AudioSettings {
    /// Output device name; None = system default.
    pub output: Option<String>,
    pub volume: f32,
    pub muted: bool,
    pub stereo: StereoMode,
    /// De-emphasis time constant: 50 us (Thailand, Europe), 75 us (Americas) or off.
    pub deemph_us: Option<f32>,
    /// AM audio bandwidth (one side), Hz.
    pub am_bw_hz: f32,
    pub eq_enabled: bool,
    pub eq_gains: [f32; 10],
    pub eq_preset: String,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            output: None,
            volume: 0.8,
            muted: false,
            stereo: StereoMode::Auto,
            deemph_us: Some(50.0),
            am_bw_hz: 4_500.0,
            eq_enabled: false,
            eq_gains: [0.0; 10],
            eq_preset: "Flat".into(),
        }
    }
}

/// Live values published by the listen session.
#[derive(Clone, Debug, Default)]
pub struct Live {
    pub pilot_locked: bool,
    pub stereo_blend: bool,
    pub pilot_level: f32,
    pub signal_db: f32,
    pub peak: [f32; 2],
    pub audio_device: Option<String>,
    pub audio_buffer_s: f64,
    pub overflows: u64,
    pub dropped_blocks: u64,
    /// (start Hz, stop Hz, dBFS bins) around the listened frequency.
    pub spectrum: Option<(f64, f64, Vec<f32>)>,
    pub streams: Vec<LiveStream>,
    /// Tuned kHz -> station kHz, confirmed by PI in the current listen session.
    pub station_aliases: BTreeMap<u32, u32>,
    pub dual_rx: Option<DualRxStatus>,
}

#[derive(Clone, Debug)]
pub struct DualRxStatus {
    pub selected: usize,
    pub pilot_db: [f32; 2],
    pub level_db: [f32; 2],
    pub samples: u64,
}

#[derive(Clone, Debug)]
pub struct RecStatus {
    pub path: PathBuf,
    pub seconds: f64,
}

pub struct RxMeasurement {
    pub channel: usize,
    pub antenna: &'static str,
    pub level_db: f32,
    pub pilot_db: f32,
    pub contrast_db: f32,
    pub clipped: bool,
}

pub struct RxSurvey {
    pub measurements: Vec<RxMeasurement>,
    pub selected: usize,
}

pub struct App {
    pub stations: BTreeMap<u32, Station>,
    pub mode: Mode,
    pub band: Band,
    /// Last frequency per band (Hz).
    pub band_freq: HashMap<Band, f64>,
    pub devices: Vec<DeviceInfo>,
    pub device_notes: Vec<String>,
    pub device_id: String,
    pub open_device: Option<String>,
    pub rescan_devices: bool,
    pub gain: f64,
    pub agc: bool,
    pub antenna: String,
    pub rx_channel: usize,
    pub rx_auto: bool,
    pub rx_dual: bool,
    pub rx1_antenna: String,
    pub rx_recheck: bool,
    pub rx_survey: Option<RxSurvey>,
    pub bias_tee: bool,
    pub audio: AudioSettings,
    pub rec: RecordSettings,
    /// Some(true) = start recording, Some(false) = stop; consumed by the worker.
    pub rec_request: Option<bool>,
    pub rec_status: Option<RecStatus>,
    pub traffic_alert: bool,
    pub tmc_names: HashMap<u16, String>,
    pub tmc_locations: crate::tmc::LocationNames,
    pub live: Live,
    pub status: String,
    pub error: Option<String>,
    pub center_hz: f64,
    pub sweeps: u64,
    pub clip: f32,
    pub log: VecDeque<String>,
    pub started: Instant,
    /// Band spectrum, kHz (25 kHz grid) -> dBFS, filled in as captures come in.
    pub spectrum: BTreeMap<u32, f32>,
    /// Frequency range (Hz) of the capture currently in progress.
    pub capture_window: Option<(f64, f64)>,
    pub settings_dirty: bool,
    pub enhance: EnhanceSettings,
    pub presets: Vec<Preset>,
    pub logbook: VecDeque<LogEntry>,
    pub streams: Vec<StreamCfg>,
    pub stream: StreamSettings,
    pub hub: Arc<Hub>,
    pub stream_status: String,
}

impl App {
    pub fn new(mode: Mode, gain: f64, antenna: String) -> Self {
        Self {
            stations: BTreeMap::new(),
            mode,
            band: Band::Fm,
            band_freq: HashMap::new(),
            devices: Vec::new(),
            device_notes: Vec::new(),
            device_id: "uhd:default".into(),
            open_device: None,
            rescan_devices: true,
            gain,
            agc: false,
            antenna,
            rx_channel: 0,
            rx_auto: false,
            rx_dual: false,
            rx1_antenna: "RX2".into(),
            rx_recheck: false,
            rx_survey: None,
            bias_tee: false,
            audio: AudioSettings::default(),
            rec: RecordSettings::default(),
            rec_request: None,
            rec_status: None,
            traffic_alert: true,
            tmc_names: HashMap::new(),
            tmc_locations: crate::tmc::LocationNames::new(),
            live: Live::default(),
            status: "Starting".into(),
            error: None,
            center_hz: 0.0,
            sweeps: 0,
            clip: 0.0,
            log: VecDeque::new(),
            started: Instant::now(),
            spectrum: BTreeMap::new(),
            capture_window: None,
            settings_dirty: false,
            enhance: EnhanceSettings::default(),
            presets: Vec::new(),
            logbook: VecDeque::new(),
            streams: Vec::new(),
            stream: StreamSettings::default(),
            hub: Hub::new(),
            stream_status: "off".into(),
        }
    }

    pub fn data_dir() -> PathBuf {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("rds-fm")
    }

    pub fn logbook_path() -> PathBuf {
        Self::data_dir().join("logbook.csv")
    }

    /// Record a reception event in memory and append it to logbook.csv.
    pub fn log_event(&mut self, freq_hz: f64, kind: &str, text: &str) {
        let e = LogEntry {
            time: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            freq_hz,
            kind: kind.to_string(),
            text: text.to_string(),
        };
        let p = Self::logbook_path();
        let new_file = !p.exists();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            if new_file {
                let _ = f.write_all("\u{feff}time,frequency_mhz,kind,text\n".as_bytes());
            }
            let _ = writeln!(f, "{},{:.4},{},\"{}\"", e.time, freq_hz / 1e6, e.kind, e.text.replace('"', "\"\""));
        }
        self.logbook.push_back(e);
        while self.logbook.len() > 5000 {
            self.logbook.pop_front();
        }
    }

    /// Load the tail of logbook.csv.
    pub fn load_logbook(&mut self) {
        let Ok(s) = std::fs::read_to_string(Self::logbook_path()) else { return };
        let lines: Vec<&str> = s.lines().skip(1).collect();
        for line in &lines[lines.len().saturating_sub(5000)..] {
            let mut it = line.splitn(4, ',');
            if let (Some(t), Some(f), Some(k), Some(x)) = (it.next(), it.next(), it.next(), it.next()) {
                self.logbook.push_back(LogEntry {
                    time: t.to_string(),
                    freq_hz: f.parse::<f64>().unwrap_or(0.0) * 1e6,
                    kind: k.to_string(),
                    text: x.trim_matches('"').replace("\"\"", "\""),
                });
            }
        }
    }

    pub fn add_preset(&mut self, name: &str, freq_hz: f64) {
        if let Some(p) = self.presets.iter_mut().find(|p| (p.freq_hz - freq_hz).abs() < 1.0) {
            if !name.is_empty() {
                p.name = name.to_string();
            }
        } else {
            self.presets.push(Preset { name: name.to_string(), freq_hz, notes: String::new() });
            self.presets.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
        }
        self.settings_dirty = true;
    }

    pub fn push_log(&mut self, msg: impl Into<String>) {
        let line = format!("[{}] {}", chrono::Local::now().format("%H:%M:%S"), msg.into());
        // A GUI-subsystem process has no stdout; println! would panic there.
        use std::io::Write;
        let _ = writeln!(std::io::stdout(), "{line}");
        self.log.push_back(line);
        while self.log.len() > 500 {
            self.log.pop_front();
        }
    }

    pub fn freq_of(&self, band: Band) -> f64 {
        self.band_freq.get(&band).copied().unwrap_or(band.default_freq())
    }

    pub fn listen(&mut self, hz: f64) {
        let band = Band::of_freq(hz);
        self.band = band;
        self.band_freq.insert(band, hz);
        self.mode = Mode::Listen { hz: hz.round() as u64, demod: band.demod() };
        self.settings_dirty = true;
    }

    pub fn selected_device(&self) -> Option<&DeviceInfo> {
        self.devices.iter().find(|d| d.id == self.device_id)
    }

    pub fn station_key(&self, tuned_khz: u32) -> u32 {
        self.live.station_aliases.get(&tuned_khz).copied().unwrap_or(tuned_khz)
    }

    pub fn stations_json(&self) -> Value {
        Value::Array(self.stations.values().map(|s| s.to_json()).collect())
    }

    pub fn to_json(&self) -> Value {
        let dual_rx = self.live.dual_rx.as_ref().map(|d| json!({
            "selected": d.selected, "pilot_db": d.pilot_db, "level_db": d.level_db, "samples": d.samples,
        }));
        let rx_survey = self.rx_survey.as_ref().map(|survey| {
            let measurements: Vec<_> = survey.measurements.iter().map(|p| json!({
                "channel": p.channel, "antenna": p.antenna, "level_db": p.level_db,
                "pilot_db": p.pilot_db, "contrast_db": p.contrast_db, "clipped": p.clipped,
            })).collect();
            json!({ "selected": survey.selected, "measurements": measurements })
        });
        json!({
            "status": self.status,
            "error": self.error,
            "mode": match self.mode { Mode::Scan => "scan", Mode::Listen { .. } => "tune", Mode::Idle => "idle" },
            "tune_mhz": match self.mode { Mode::Listen { hz, .. } => Some(hz as f64 / 1e6), _ => None },
            "center_mhz": self.center_hz / 1e6,
            "gain": self.gain,
            "antenna": self.antenna,
            "rx_channel": self.rx_channel,
            "rx_auto": self.rx_auto,
            "rx_survey": rx_survey,
            "rx_dual": self.rx_dual,
            "rx1_antenna": self.rx1_antenna,
            "dual_rx": dual_rx,
            "sweeps": self.sweeps,
            "clip": self.clip,
            "uptime_s": self.started.elapsed().as_secs(),
            "log": self.log,
            "device": self.selected_device().map(|d| d.label.clone()),
            "live": {
                "pilot_locked": self.live.pilot_locked,
                "stereo": self.live.stereo_blend,
                "pilot_level": self.live.pilot_level,
                "signal_db": self.live.signal_db,
                "peak": self.live.peak.to_vec(),
                "audio_device": self.live.audio_device,
                "audio_buffer_s": self.live.audio_buffer_s,
                "dropped_blocks": self.live.dropped_blocks,
            },
            "recording": self.rec_status.as_ref().map(|r| json!({"path": r.path, "seconds": r.seconds})),
            "stations": self.stations_json(),
        })
    }

    // ------------------------------------------------------------ settings

    pub fn settings_path() -> PathBuf {
        std::env::var_os("APPDATA")
            .map(|a| PathBuf::from(a).join("rds-fm"))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("settings.json")
    }

    pub fn settings_json(&self) -> Value {
        let a = &self.audio;
        let r = &self.rec;
        json!({
            "device_id": self.device_id,
            "band": self.band.key(),
            "band_freq": self.band_freq.iter().map(|(b, f)| (b.key().to_string(), json!(f))).collect::<serde_json::Map<_, _>>(),
            "gain": self.gain, "agc": self.agc, "antenna": self.antenna, "bias_tee": self.bias_tee,
            "rx_channel": self.rx_channel,
            "rx_auto": self.rx_auto,
            "rx_dual": self.rx_dual,
            "rx1_antenna": self.rx1_antenna,
            "traffic_alert": self.traffic_alert,
            "audio": {
                "output": a.output, "volume": a.volume, "muted": a.muted,
                "stereo": matches!(a.stereo, StereoMode::Auto), "deemph_us": a.deemph_us,
                "am_bw_hz": a.am_bw_hz, "eq_enabled": a.eq_enabled, "eq_gains": a.eq_gains.to_vec(),
                "eq_preset": a.eq_preset,
            },
            "record": {
                "format": r.format.name(), "wav_bits": r.wav_bits, "flac_bits": r.flac_bits,
                "mp3_kbps": r.mp3_kbps, "aac_kbps": r.aac_kbps, "aac_he": r.aac_he,
                "sample_rate": r.sample_rate, "stereo": r.stereo, "folder": r.folder,
            },
            "enhance": {
                "denoise": self.enhance.denoise, "denoise_mix": self.enhance.denoise_mix,
                "bwe": self.enhance.bwe, "bwe_amount": self.enhance.bwe_amount, "width": self.enhance.width,
            },
            "presets": self.presets.iter().map(|p| json!({"name": p.name, "freq_hz": p.freq_hz, "notes": p.notes})).collect::<Vec<_>>(),
            "streams": self.streams.iter().map(|s| json!({"freq_hz": s.freq_hz, "enabled": s.enabled})).collect::<Vec<_>>(),
            "stream": {
                "server": self.stream.server, "bind": self.stream.bind, "kbps": self.stream.kbps,
                "stream_main": self.stream.stream_main,
                "icecast": {
                    "enabled": self.stream.icecast.enabled, "host": self.stream.icecast.host,
                    "port": self.stream.icecast.port, "user": self.stream.icecast.user,
                    "password": self.stream.icecast.password,
                },
            },
        })
    }

    pub fn load_settings(&mut self, v: &Value) {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let f = |k: &str| v.get(k).and_then(Value::as_f64);
        let b = |k: &str| v.get(k).and_then(Value::as_bool);
        if let Some(x) = s("device_id") {
            self.device_id = x;
        }
        if let Some(x) = s("band").and_then(|k| Band::from_key(&k)) {
            self.band = x;
        }
        if let Some(m) = v.get("band_freq").and_then(Value::as_object) {
            for (k, val) in m {
                if let (Some(band), Some(hz)) = (Band::from_key(k), val.as_f64()) {
                    self.band_freq.insert(band, hz);
                }
            }
        }
        self.gain = f("gain").unwrap_or(self.gain);
        self.agc = b("agc").unwrap_or(self.agc);
        self.antenna = s("antenna").unwrap_or(self.antenna.clone());
        self.rx_channel = v.get("rx_channel").and_then(Value::as_u64).filter(|&n| n <= 1).unwrap_or(0) as usize;
        self.rx_auto = b("rx_auto").unwrap_or(false);
        self.rx_dual = b("rx_dual").unwrap_or(false);
        self.rx1_antenna = s("rx1_antenna").filter(|a| a == "TX/RX" || a == "RX2").unwrap_or_else(|| "RX2".into());
        self.bias_tee = b("bias_tee").unwrap_or(false);
        self.traffic_alert = b("traffic_alert").unwrap_or(true);
        if let Some(a) = v.get("audio") {
            let au = &mut self.audio;
            au.output = a.get("output").and_then(Value::as_str).map(str::to_string);
            au.volume = a.get("volume").and_then(Value::as_f64).unwrap_or(0.8) as f32;
            au.muted = a.get("muted").and_then(Value::as_bool).unwrap_or(false);
            au.stereo = if a.get("stereo").and_then(Value::as_bool).unwrap_or(true) { StereoMode::Auto } else { StereoMode::Mono };
            au.deemph_us = a.get("deemph_us").and_then(Value::as_f64).map(|x| x as f32);
            au.am_bw_hz = a.get("am_bw_hz").and_then(Value::as_f64).unwrap_or(4500.0) as f32;
            au.eq_enabled = a.get("eq_enabled").and_then(Value::as_bool).unwrap_or(false);
            if let Some(g) = a.get("eq_gains").and_then(Value::as_array) {
                for (i, x) in g.iter().take(10).enumerate() {
                    au.eq_gains[i] = x.as_f64().unwrap_or(0.0) as f32;
                }
            }
            au.eq_preset = a.get("eq_preset").and_then(Value::as_str).unwrap_or("Flat").to_string();
        }
        if let Some(r) = v.get("record") {
            let rc = &mut self.rec;
            rc.format = r.get("format").and_then(Value::as_str).and_then(Format::from_name).unwrap_or(rc.format);
            let u = |k: &str, d: u64| r.get(k).and_then(Value::as_u64).unwrap_or(d);
            rc.wav_bits = u("wav_bits", rc.wav_bits as u64) as u16;
            rc.flac_bits = u("flac_bits", rc.flac_bits as u64) as u16;
            rc.mp3_kbps = u("mp3_kbps", rc.mp3_kbps as u64) as u32;
            rc.aac_kbps = u("aac_kbps", rc.aac_kbps as u64) as u32;
            rc.aac_he = r.get("aac_he").and_then(Value::as_bool).unwrap_or(false);
            rc.sample_rate = u("sample_rate", rc.sample_rate as u64) as u32;
            rc.stereo = r.get("stereo").and_then(Value::as_bool).unwrap_or(true);
            if let Some(p) = r.get("folder").and_then(Value::as_str) {
                rc.folder = PathBuf::from(p);
            }
        }
        if let Some(e) = v.get("enhance") {
            let en = &mut self.enhance;
            en.denoise = e.get("denoise").and_then(Value::as_bool).unwrap_or(false);
            en.denoise_mix = e.get("denoise_mix").and_then(Value::as_f64).unwrap_or(0.7) as f32;
            en.bwe = e.get("bwe").and_then(Value::as_bool).unwrap_or(false);
            en.bwe_amount = e.get("bwe_amount").and_then(Value::as_f64).unwrap_or(0.35) as f32;
            en.width = e.get("width").and_then(Value::as_f64).unwrap_or(1.0) as f32;
        }
        if let Some(a) = v.get("presets").and_then(Value::as_array) {
            self.presets = a
                .iter()
                .filter_map(|p| {
                    Some(Preset {
                        name: p.get("name")?.as_str()?.to_string(),
                        freq_hz: p.get("freq_hz")?.as_f64()?,
                        notes: p.get("notes").and_then(Value::as_str).unwrap_or("").to_string(),
                    })
                })
                .collect();
        }
        if let Some(a) = v.get("streams").and_then(Value::as_array) {
            self.streams = a
                .iter()
                .filter_map(|s| Some(StreamCfg { freq_hz: s.get("freq_hz")?.as_f64()?, enabled: s.get("enabled")?.as_bool()? }))
                .collect();
        }
        if let Some(s) = v.get("stream") {
            let st = &mut self.stream;
            st.server = s.get("server").and_then(Value::as_bool).unwrap_or(false);
            st.bind = s.get("bind").and_then(Value::as_str).unwrap_or("0.0.0.0:8000").to_string();
            st.kbps = s.get("kbps").and_then(Value::as_u64).unwrap_or(128) as u32;
            st.stream_main = s.get("stream_main").and_then(Value::as_bool).unwrap_or(true);
            if let Some(i) = s.get("icecast") {
                let ic = &mut st.icecast;
                ic.enabled = i.get("enabled").and_then(Value::as_bool).unwrap_or(false);
                ic.host = i.get("host").and_then(Value::as_str).unwrap_or("localhost").to_string();
                ic.port = i.get("port").and_then(Value::as_u64).unwrap_or(8000) as u16;
                ic.user = i.get("user").and_then(Value::as_str).unwrap_or("source").to_string();
                ic.password = i.get("password").and_then(Value::as_str).unwrap_or("").to_string();
            }
        }
    }

    pub fn save_settings(&mut self) {
        let p = Self::settings_path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(s) = serde_json::to_string_pretty(&self.settings_json()) {
            let _ = std::fs::write(&p, s);
        }
        self.settings_dirty = false;
    }

    pub fn load_settings_file(&mut self) {
        if let Ok(s) = std::fs::read_to_string(Self::settings_path()) {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                self.load_settings(&v);
            }
        }
    }
}
