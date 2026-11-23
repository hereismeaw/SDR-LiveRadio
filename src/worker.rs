//! Radio worker: owns the SDR, runs FM band scans (burst captures) or a continuous
//! listen session (audio + RDS), and merges results into `App`.

use crate::app::{App, Band, Demod, LiveStream, Mode, RecStatus};
use crate::audio::{drift_adjust, AudioOut, Equalizer, Frame, Resampler};
use crate::broadcast::{mount_name, spawn_icecast, LiveMp3};
use crate::enhance::Enhancer;
use crate::demod::{AmDemod, FmDiscriminator, StereoDecoder};
use crate::device::{self, DeviceInfo, DriverPaths, Kind, Sdr};
use crate::dsp::{self, Dsp, MPX_LEN, MPX_RATE};
use crate::pipeline::{self, CaptureOutput, StationResult};
use crate::rds::find_groups;
use crate::record::{Recorder, Tags};
use crate::station::Station;
use crate::stream::Channelizer;
use num_complex::Complex32 as C32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Listen sample rate for both devices: 5 x 228 kHz.
const LISTEN_RATE: f64 = MPX_RATE * 5.0;

pub struct WorkerCfg {
    pub paths: DriverPaths,
    pub save_path: PathBuf,
}

/// FM scan plan for a device: (IQ rate, half window, capture centres).
fn scan_plan(kind: Kind) -> (f64, f64, Vec<f64>) {
    match kind {
        // 4.56 MS/s sc16 = 18 MB/s: safe on USB 2.0, ~4 MHz per capture
        Kind::Uhd => (MPX_RATE * 20.0, 2.0e6, (0..6).map(|i| 89.375e6 + 4e6 * i as f64).collect()),
        // RTL-SDR: 2.28 MS/s, ~2 MHz per capture
        Kind::RtlSdr => (MPX_RATE * 10.0, 1.0e6, (0..11).map(|i| 88.375e6 + 2e6 * i as f64).collect()),
    }
}

fn hint(e: &str) -> String {
    if e.contains("No devices found") || e.contains("LookupError") {
        format!("{e}\n→ B210 not found: close SDRangel or any other program using the device, and check the USB cable")
    } else if e.contains("uhd.dll") {
        format!("{e}\n→ check that UHD is installed, or pass its location with --uhd-dll")
    } else if e.contains("rtlsdr") {
        format!("{e}\n→ RTL-SDR: plug it in, install the WinUSB driver with Zadig, close other SDR programs")
    } else {
        e.to_string()
    }
}

struct Open {
    id: String,
    kind: Kind,
    sdr: Sdr,
    rate: f64,
    gain: Option<Option<f64>>,
    antenna: Option<String>,
    bias: Option<bool>,
}

impl Open {
    fn ensure_rate(&mut self, rate: f64) -> Result<(), String> {
        if self.rate != rate {
            let got = self.sdr.set_rate(rate)?;
            if (got - rate).abs() > 1.0 {
                return Err(format!("got sample rate {got} Hz instead of {rate} Hz"));
            }
            self.rate = rate;
        }
        Ok(())
    }

    /// Apply gain / antenna / bias-tee from App if they changed.
    fn apply_controls(&mut self, app: &Arc<Mutex<App>>) -> Result<(), String> {
        let (g, ant, bias) = {
            let a = app.lock().unwrap();
            (if a.agc { None } else { Some(a.gain) }, a.antenna.clone(), a.bias_tee)
        };
        if self.gain != Some(g) {
            self.sdr.set_gain(g)?;
            self.gain = Some(g);
        }
        if self.antenna.as_deref() != Some(ant.as_str()) {
            self.sdr.set_antenna(&ant)?;
            self.antenna = Some(ant);
        }
        if self.bias != Some(bias) {
            self.sdr.set_bias_tee(bias)?;
            self.bias = Some(bias);
        }
        Ok(())
    }
}

fn merge(app: &Arc<Mutex<App>>, out: CaptureOutput, force: bool) {
    let mut a = app.lock().unwrap();
    let clip = out.clip;
    a.clip = clip;
    for (f, db) in out.spectrum {
        a.spectrum.insert((f / 1e3).round() as u32, db);
    }
    let mut events = Vec::new();
    for r in out.stations {
        let mut khz = (r.freq_hz / 1e3).round() as u32;
        // The detected peak can wobble by one 50 kHz grid step between sweeps: reuse a
        // known station 50 kHz away instead of creating a twin (scan mode only).
        if !force && !a.stations.contains_key(&khz) {
            if let Some(&k) = [khz - 50, khz + 50].iter().find(|k| a.stations.contains_key(k)) {
                khz = k;
            }
        }
        let mhz = khz as f64 / 1000.0;
        // A spectral bump with no stereo pilot, no 57 kHz energy and no RDS groups is
        // noise / interference, not a broadcast station.
        let looks_like_fm = r.pilot_db >= 10.0 || r.rds_snr_db >= 3.0 || !r.groups.is_empty();
        if !a.stations.contains_key(&khz) && !looks_like_fm && !force {
            continue;
        }
        let mut book: Vec<(String, String)> = Vec::new();
        if !a.stations.contains_key(&khz) {
            events.push(format!("{mhz:.2} MHz  new station ({:.0} dBFS)", r.level_db));
            book.push(("station".into(), format!("new station, {:.0} dBFS, pilot {:.0} dB", r.level_db, r.pilot_db)));
        }
        let st = a.stations.entry(khz).or_insert_with(|| Station::new(khz));
        st.update_metrics(r.level_db, r.pilot_db, r.rds_snr_db, r.stats);
        for g in &r.groups {
            for ev in st.apply_group(g) {
                events.push(format!("{mhz:.2} MHz  {ev}"));
                let kind = ev.split_whitespace().next().unwrap_or("rds").trim_end_matches(':').to_lowercase();
                let kind = match kind.as_str() {
                    "traffic" => "ta".to_string(),
                    k => k.to_string(),
                };
                book.push((kind, ev));
            }
        }
        let pi = st.pi.map(|p| format!(" [PI {p:04X}]")).unwrap_or_default();
        for (k, t) in book {
            a.log_event(khz as f64 * 1e3, &k, &format!("{t}{pi}"));
        }
    }
    if clip > 0.001 {
        events.push(format!("Signal too strong (clip {:.2}%) — lower the gain", clip * 100.0));
    }
    for e in events {
        a.push_log(e);
    }
}

/// Keeps the built-in stream server in line with the settings.
pub fn run_stream_server(app: Arc<Mutex<App>>) {
    let mut server: Option<crate::broadcast::Server> = None;
    let mut cur: Option<String> = None;
    loop {
        let (want, bind, hub) = {
            let a = app.lock().unwrap();
            (a.stream.server, a.stream.bind.clone(), a.hub.clone())
        };
        let target = want.then_some(bind.clone());
        if target != cur {
            server = None;
            cur = target.clone();
            let status = match target {
                Some(b) => match crate::broadcast::Server::start(hub, &b) {
                    Ok(s) => {
                        let st = format!("on air at http://{}/", local_url(&b, s.port));
                        server = Some(s);
                        st
                    }
                    Err(e) => e,
                },
                None => "off".into(),
            };
            let mut a = app.lock().unwrap();
            a.push_log(format!("Stream server: {status}"));
            a.stream_status = status;
        }
        let _ = &server;
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// Human URL for a bind address (0.0.0.0 -> this PC's LAN address).
pub fn local_url(bind: &str, port: u16) -> String {
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or("0.0.0.0");
    if host == "0.0.0.0" || host == "::" || host.is_empty() {
        // the address used for the default route is the one other machines can reach
        let ip = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| s.connect("8.8.8.8:80").map(|_| s))
            .and_then(|s| s.local_addr())
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|_| "127.0.0.1".into());
        format!("{ip}:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn save_stations(app: &Arc<Mutex<App>>, path: &PathBuf) {
    let json = app.lock().unwrap().stations_json();
    if let Ok(s) = serde_json::to_string_pretty(&json) {
        let _ = std::fs::write(path, s);
    }
}

fn scan_step(dev: &mut Open, dsp: &mut Dsp, app: &Arc<Mutex<App>>, chunk: &mut usize, cfg: &WorkerCfg) -> Result<(), String> {
    let (rate, half, centers) = scan_plan(dev.kind);
    dev.ensure_rate(rate)?;
    let idx = *chunk % centers.len();
    let c = centers[idx];
    {
        let mut a = app.lock().unwrap();
        a.status = format!(
            "Scanning {:.1}–{:.1} MHz (segment {}/{}, sweep {})",
            (c - half).max(87.5e6) / 1e6,
            (c + half).min(108.0e6) / 1e6,
            idx + 1,
            centers.len(),
            a.sweeps + 1
        );
        a.capture_window = Some((c - half, c + half));
    }
    let actual = dev.sdr.tune(c)?;
    app.lock().unwrap().center_hz = actual;
    let n = pipeline::capture_len(rate)?;
    let iq = dev.sdr.capture(n, (rate * 0.05) as usize)?;
    let out = pipeline::process_capture(dsp, iq, rate, actual, c - half, c + half)?;
    merge(app, out, false);
    *chunk += 1;
    if *chunk % centers.len() == 0 {
        let sweeps = {
            let mut a = app.lock().unwrap();
            a.sweeps += 1;
            a.sweeps
        };
        save_stations(app, &cfg.save_path);
        app.lock().unwrap().push_log(format!("Sweep {sweeps} complete, saved {}", cfg.save_path.display()));
    }
    Ok(())
}

enum Block {
    Data(Vec<C32>),
    Err(String),
}

/// Program audio rate: EQ, enhancer, recorder and stream encoders all run at 48 kHz.
const PROGRAM_RATE: f64 = 48_000.0;

/// One demodulated channel of a listen session.
struct Chan {
    freq: f64,
    khz: u32,
    main: bool,
    disc: FmDiscriminator,
    stereo: StereoDecoder,
    am: AmDemod,
    up: Resampler,
    eq: Equalizer,
    enh: Enhancer,
    rds_acc: Vec<f32>,
    mount: Option<String>,
    mp3: Option<LiveMp3>,
    title: String,
    pwr: f32,
}

impl Chan {
    fn new(freq: f64, main: bool, audio_rate: f64, cutoff: f64) -> Self {
        Self {
            freq,
            khz: (freq / 1e3).round() as u32,
            main,
            disc: FmDiscriminator::default(),
            stereo: StereoDecoder::new(),
            am: AmDemod::new(),
            up: Resampler::new(audio_rate, PROGRAM_RATE),
            eq: Equalizer::new(PROGRAM_RATE),
            enh: Enhancer::new(cutoff),
            rds_acc: Vec::with_capacity(MPX_LEN * 2),
            mount: None,
            mp3: None,
            title: String::new(),
            pwr: 0.0,
        }
    }
}

/// Session layout: IQ rate, LO frequency, channel frequencies (main first) and
/// stream frequencies that could not be included.
fn plan_session(kind: Kind, main: f64, demod: Demod, extras: &[f64]) -> (f64, f64, Vec<f64>, Vec<f64>) {
    let offset = if demod == Demod::Wfm { 250e3 } else { 200e3 };
    if demod == Demod::Am || extras.is_empty() {
        return (LISTEN_RATE, main + offset, vec![main], extras.to_vec());
    }
    let rate = match kind {
        Kind::Uhd => MPX_RATE * 20.0,   // 4.56 MS/s: ~3.7 MHz of usable band
        Kind::RtlSdr => MPX_RATE * 10.0, // 2.28 MS/s: ~1.8 MHz
    };
    let usable = rate / 2.0 - 140e3;
    let mut sel = vec![main];
    let mut skipped = Vec::new();
    let mut ex: Vec<f64> = extras.to_vec();
    ex.sort_by(|a, b| (a - main).abs().total_cmp(&(b - main).abs()));
    for f in ex {
        let lo = sel.iter().copied().fold(f, f64::min);
        let hi = sel.iter().copied().fold(f, f64::max);
        if hi - lo <= 2.0 * usable - 260e3 {
            sel.push(f);
        } else {
            skipped.push(f);
        }
    }
    let lo_f = sel.iter().copied().fold(f64::MAX, f64::min);
    let hi_f = sel.iter().copied().fold(f64::MIN, f64::max);
    let mut lo = (lo_f + hi_f) / 2.0;
    // keep every channel at least 70 kHz away from the DC spike
    for shift in [0.0, 130e3, -130e3, 260e3, -260e3] {
        let c = (lo_f + hi_f) / 2.0 + shift;
        if sel.iter().all(|f| (f - c).abs() > 70e3 && (f - c).abs() < usable) {
            lo = c;
            break;
        }
    }
    (rate, lo, sel, skipped)
}

fn now_playing(st: Option<&Station>, label: &str) -> String {
    let Some(s) = st else { return label.to_string() };
    if let (Some(t), Some(a)) = (s.rtplus.tags.get(&1), s.rtplus.tags.get(&4)) {
        return format!("{a} - {t}");
    }
    let (ps, _) = s.ps_display();
    let (rt, _) = s.rt_display();
    match (ps.trim().is_empty() || ps.contains('_'), rt.trim().is_empty()) {
        (false, false) => format!("{} - {}", ps.trim(), rt.trim()),
        (false, true) => format!("{} ({label})", ps.trim()),
        (true, false) => rt.trim().to_string(),
        (true, true) => label.to_string(),
    }
}

/// Continuous reception with audio output, recording, RDS and internet streaming.
/// Extra FM stations from the stream list are demodulated from the same capture when
/// they fit in the device bandwidth. Returns when mode, frequency or device changes.
fn listen_session(dev: Open, dsp: &mut Dsp, app: &Arc<Mutex<App>>, hz: u64, demod: Demod, cfg: &WorkerCfg) -> (Open, Result<(), String>) {
    let mut dev = dev;
    let f = hz as f64;
    let (streams, stream_set, enh_set0, am_bw) = {
        let a = app.lock().unwrap();
        (a.streams.clone(), a.stream.clone(), a.enhance.clone(), a.audio.am_bw_hz as f64)
    };
    // stream only when something can carry the audio out
    let streaming = stream_set.server || stream_set.icecast.enabled;
    let want: Vec<f64> = if streaming { streams.iter().filter(|s| s.enabled).map(|s| s.freq_hz).collect() } else { Vec::new() };
    let main_streamed = streaming && (stream_set.stream_main || want.iter().any(|x| (x - f).abs() < 1.0));
    let extras: Vec<f64> = if demod == Demod::Wfm {
        want.iter().copied().filter(|x| (x - f).abs() >= 1.0 && Band::of_freq(*x) == Band::Fm).collect()
    } else {
        Vec::new()
    };
    let (rate, lo_target, freqs, skipped) = plan_session(dev.kind, f, demod, &extras);
    let setup = (|| -> Result<f64, String> {
        dev.ensure_rate(rate)?;
        dev.apply_controls(app)?;
        let lo = dev.sdr.tune(lo_target)?;
        dev.sdr.start_stream()?;
        Ok(lo)
    })();
    let lo = match setup {
        Ok(lo) => lo,
        Err(e) => return (dev, Err(e)),
    };
    let (mut ch, audio_rate, cutoff) = match demod {
        Demod::Wfm => {
            let d = (rate / MPX_RATE).round() as usize;
            let spec: Vec<(f64, f64, f64)> = freqs.iter().map(|x| (x - lo, 100e3, 114e3)).collect();
            (Channelizer::new_multi(rate, d, 1 << 15, 1 << 11, &spec), StereoDecoder::OUT_RATE, 15_000.0)
        }
        Demod::Am => {
            let c = Channelizer::new(rate, 24, 1 << 13, 1 << 9, f - lo, am_bw, am_bw + 1_500.0);
            let r = c.out_rate;
            (c, r, am_bw)
        }
    };
    let label = match demod {
        Demod::Wfm => format!("{:.2} MHz FM", f / 1e6),
        Demod::Am if f < 30e6 => format!("{:.0} kHz AM", f / 1e3),
        Demod::Am => format!("{:.3} MHz AM", f / 1e6),
    };
    let mut chans: Vec<Chan> = freqs.iter().enumerate().map(|(i, &x)| Chan::new(x, i == 0, audio_rate, cutoff)).collect();

    // streaming: open hub mounts and (optionally) Icecast pushes
    let hub = app.lock().unwrap().hub.clone();
    let ice_stop = Arc::new(AtomicBool::new(false));
    let mut live_streams = Vec::new();
    for c in chans.iter_mut() {
        if c.main && !main_streamed {
            continue;
        }
        let mount = mount_name(c.freq);
        match LiveMp3::new(stream_set.kbps, 48_000) {
            Ok(enc) => {
                let name = if c.main { label.clone() } else { format!("{:.2} MHz FM", c.freq / 1e6) };
                hub.open(&mount, &name, stream_set.kbps);
                if stream_set.icecast.enabled {
                    let app2 = app.clone();
                    let log: crate::broadcast::Log = Arc::new(move |m| app2.lock().unwrap().push_log(m));
                    spawn_icecast(hub.clone(), stream_set.icecast.clone(), mount.clone(), ice_stop.clone(), log);
                }
                live_streams.push(LiveStream { freq_hz: c.freq, mount: mount.clone(), on_air: true, note: String::new() });
                c.mount = Some(mount);
                c.mp3 = Some(enc);
            }
            Err(e) => app.lock().unwrap().push_log(format!("MP3 encoder: {e}")),
        }
    }
    for s in &skipped {
        live_streams.push(LiveStream {
            freq_hz: *s,
            mount: mount_name(*s),
            on_air: false,
            note: format!("outside the {:.1} MHz the device can capture with {label}", rate / 1e6 - 0.28),
        });
    }
    for s in streams.iter().filter(|s| s.enabled && demod == Demod::Am && (s.freq_hz - f).abs() >= 1.0) {
        live_streams.push(LiveStream { freq_hz: s.freq_hz, mount: mount_name(s.freq_hz), on_air: false, note: "multi-station streaming is FM only".into() });
    }

    let out_name = app.lock().unwrap().audio.output.clone();
    let out = match AudioOut::open(out_name.as_deref()) {
        Ok(o) => Some(o),
        Err(e) => {
            app.lock().unwrap().push_log(format!("Audio output unavailable: {e}"));
            None
        }
    };
    {
        let mut a = app.lock().unwrap();
        a.status = if chans.len() > 1 { format!("Listening {label} (+{} streamed)", chans.len() - 1) } else { format!("Listening {label}") };
        a.center_hz = lo;
        a.capture_window = Some((lo - rate / 2.0, lo + rate / 2.0));
        a.live = Default::default();
        a.live.audio_device = out.as_ref().map(|o| o.device_name.clone());
        a.live.streams = live_streams.clone();
        a.push_log(format!(
            "Listening {label}{}{}",
            out.as_ref().map(|o| format!(" → {} ({} Hz)", o.device_name, o.rate)).unwrap_or_default(),
            if live_streams.iter().any(|s| s.on_air) {
                format!(", streaming {}", live_streams.iter().filter(|s| s.on_air).map(|s| format!("/{}", s.mount)).collect::<Vec<_>>().join(" "))
            } else {
                String::new()
            }
        ));
        for s in live_streams.iter().filter(|s| !s.on_air) {
            a.push_log(format!("Stream {:.2} MHz not started: {}", s.freq_hz / 1e6, s.note));
        }
    }

    // Reader thread owns the device while streaming.
    let stop = Arc::new(AtomicBool::new(false));
    let ctrl: Arc<Mutex<Option<(Option<f64>, String, bool)>>> = Arc::new(Mutex::new(None));
    let (tx, rx) = sync_channel::<Block>(64);
    let dropped = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let reader = {
        let stop = stop.clone();
        let ctrl = ctrl.clone();
        let dropped = dropped.clone();
        std::thread::spawn(move || {
            let mut dev = dev;
            let mut buf = vec![C32::default(); 1 << 16];
            while !stop.load(Ordering::Relaxed) {
                if let Some((g, ant, bias)) = ctrl.lock().unwrap().take() {
                    let _ = dev.sdr.set_gain(g);
                    let _ = dev.sdr.set_antenna(&ant);
                    let _ = dev.sdr.set_bias_tee(bias);
                    dev.gain = Some(g);
                    dev.antenna = Some(ant);
                    dev.bias = Some(bias);
                }
                match dev.sdr.read(&mut buf) {
                    Ok(0) => {}
                    Ok(n) => {
                        if let Err(TrySendError::Full(_)) = tx.try_send(Block::Data(buf[..n].to_vec())) {
                            dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Block::Err(e));
                        break;
                    }
                }
            }
            dev.sdr.stop_stream();
            dev
        })
    };

    let mut out_rs = out.as_ref().map(|o| Resampler::new(PROGRAM_RATE, o.rate as f64));
    let mut recorder: Option<Recorder> = None;
    let mut outs: Vec<Vec<C32>> = vec![Vec::new(); chans.len()];
    let mut mpx = Vec::new();
    let mut native: Vec<Frame> = Vec::new();
    let mut a48: Vec<Frame> = Vec::new();
    let mut outbuf: Vec<Frame> = Vec::new();
    let mut last_spec = Instant::now();
    let mut last_save = Instant::now();
    let mut last_title = Instant::now();
    let mut cur_am_bw = am_bw;
    let mut cur_ctrl = dev_ctrl_snapshot(app);
    let session_streams = (streams.clone(), stream_set.clone());
    let result: Result<(), String> = loop {
        let blk = match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Block::Data(d)) => d,
            Ok(Block::Err(e)) => break Err(e),
            Err(RecvTimeoutError::Timeout) => break Err("no samples from the device for 3 s".into()),
            Err(RecvTimeoutError::Disconnected) => break Err("device reader stopped".into()),
        };
        // settings snapshot
        let (mode, dev_id, rescan, audio_set, enh_set, rec_req, restart) = {
            let mut a = app.lock().unwrap();
            let rec_req = a.rec_request.take();
            let restart = (a.streams.clone(), a.stream.clone()) != session_streams;
            (a.mode, a.device_id.clone(), a.rescan_devices, a.audio.clone(), a.enhance.clone(), rec_req, restart)
        };
        if mode != (Mode::Listen { hz, demod }) || rescan || dev_id != app_open_id(app) || restart {
            break Ok(());
        }
        let snap = dev_ctrl_snapshot(app);
        if snap != cur_ctrl {
            *ctrl.lock().unwrap() = Some(snap.clone());
            cur_ctrl = snap;
        }
        if demod == Demod::Am && (audio_set.am_bw_hz as f64 - cur_am_bw).abs() > 1.0 {
            cur_am_bw = audio_set.am_bw_hz as f64;
            ch.set_filter(0, f - lo, cur_am_bw, cur_am_bw + 1_500.0);
            chans[0].enh.set_cutoff(cur_am_bw);
        }
        match rec_req {
            Some(true) if recorder.is_none() => {
                let (rec_settings, ps, rt) = {
                    let a = app.lock().unwrap();
                    let st = a.stations.get(&chans[0].khz);
                    (a.rec.clone(), st.map(|s| s.ps_display().0).unwrap_or_default(), st.map(|s| s.rt_display().0).unwrap_or_default())
                };
                let name = format!(
                    "{} {}{}",
                    chrono::Local::now().format("%Y-%m-%d %H%M%S"),
                    label,
                    if ps.trim().is_empty() || ps.contains('_') || ps.starts_with('(') { String::new() } else { format!(" {}", ps.trim()) }
                );
                let tags = Tags {
                    title: if rt.is_empty() { label.clone() } else { rt.clone() },
                    artist: if ps.trim().is_empty() { label.clone() } else { ps.trim().to_string() },
                    comment: label.clone(),
                };
                let app2 = app.clone();
                let log: crate::record::Log = Arc::new(move |m| app2.lock().unwrap().push_log(m));
                match Recorder::start(&rec_settings, PROGRAM_RATE, &name, tags, log) {
                    Ok(r) => {
                        let mut a = app.lock().unwrap();
                        a.push_log(format!("Recording → {}", r.path.display()));
                        a.log_event(f, "record", &format!("recording started: {}", r.path.display()));
                        recorder = Some(r);
                    }
                    Err(e) => app.lock().unwrap().push_log(format!("Cannot start recording: {e}")),
                }
            }
            Some(false) => {
                if let Some(r) = recorder.take() {
                    r.stop();
                }
            }
            _ => {}
        }

        // DSP
        if last_spec.elapsed() > Duration::from_millis(350) {
            ch.request_spectrum();
            last_spec = Instant::now();
        }
        outs.iter_mut().for_each(Vec::clear);
        ch.push_multi(dsp, &blk, &mut outs);
        if outs[0].is_empty() {
            continue;
        }
        let mut peak = [0f32; 2];
        let mut rds_results = Vec::new();
        for (i, c) in chans.iter_mut().enumerate() {
            let base = &outs[i];
            c.pwr = base.iter().map(|v| v.norm_sqr()).sum::<f32>() / base.len().max(1) as f32;
            native.clear();
            match demod {
                Demod::Wfm => {
                    mpx.clear();
                    c.disc.process(base, &mut mpx);
                    let mode = if c.main { audio_set.stereo } else { crate::demod::StereoMode::Auto };
                    c.stereo.process(&mpx, mode, audio_set.deemph_us, &mut native);
                    c.rds_acc.extend_from_slice(&mpx);
                }
                Demod::Am => c.am.process(base, &mut native),
            }
            a48.clear();
            c.up.process(&native, &mut a48);
            c.eq.set(audio_set.eq_gains, audio_set.eq_enabled);
            c.eq.process(&mut a48);
            c.enh.process(&enh_set, &mut a48);
            if let (Some(m), Some(enc)) = (&c.mount, c.mp3.as_mut()) {
                hub.publish(m, enc.encode(&a48));
            }
            if c.main {
                if let Some(r) = &recorder {
                    r.push(&a48);
                }
                let vol = if audio_set.muted { 0.0 } else { audio_set.volume };
                for fr in a48.iter_mut() {
                    fr[0] *= vol;
                    fr[1] *= vol;
                    peak[0] = peak[0].max(fr[0].abs());
                    peak[1] = peak[1].max(fr[1].abs());
                }
                if let (Some(o), Some(rs)) = (&out, out_rs.as_mut()) {
                    rs.set_adjust(drift_adjust(o.buffered_seconds(), 0.25));
                    outbuf.clear();
                    rs.process(&a48, &mut outbuf);
                    o.push(&outbuf);
                }
            }
            // RDS every 2.3 s of MPX
            if demod == Demod::Wfm && c.rds_acc.len() >= MPX_LEN {
                let block: Vec<f32> = c.rds_acc.drain(..MPX_LEN).collect();
                let info = dsp.analyze_mpx(&block);
                let bits = dsp::rds_bits(&info.rds_bb, info.pilot_bb.as_deref());
                let (groups, stats) = find_groups(&bits);
                rds_results.push(StationResult {
                    freq_hz: c.freq,
                    level_db: 10.0 * (c.pwr + 1e-30).log10(),
                    pilot_db: info.pilot_db,
                    rds_snr_db: info.rds_snr_db,
                    groups,
                    stats,
                });
            }
        }

        let spec = ch.take_spectrum();
        {
            let mut a = app.lock().unwrap();
            let main = &chans[0];
            let l = &mut a.live;
            l.pilot_locked = demod == Demod::Wfm && main.stereo.pilot_locked;
            l.stereo_blend = l.pilot_locked && audio_set.stereo == crate::demod::StereoMode::Auto;
            l.pilot_level = main.stereo.pilot_level;
            l.signal_db = 10.0 * (main.pwr + 1e-30).log10();
            l.peak = peak;
            l.audio_buffer_s = out.as_ref().map(|o| o.buffered_seconds()).unwrap_or(0.0);
            l.dropped_blocks = dropped.load(Ordering::Relaxed);
            if let Some(mut s) = spec {
                // exponential average in linear power: FM looks like a band, not a few spikes
                if let Some((_, _, prev)) = &l.spectrum {
                    if prev.len() == s.len() {
                        for (v, p) in s.iter_mut().zip(prev) {
                            *v = 10.0 * (0.3 * 10f32.powf(*v / 10.0) + 0.7 * 10f32.powf(*p / 10.0)).log10();
                        }
                    }
                }
                l.spectrum = Some((lo - rate / 2.0, lo + rate / 2.0, s));
            }
            a.rec_status = recorder.as_ref().map(|r| RecStatus { path: r.path.clone(), seconds: r.seconds() });
            // stream titles from RDS, once a second
            if last_title.elapsed() > Duration::from_secs(1) {
                last_title = Instant::now();
                for c in chans.iter_mut() {
                    if let Some(m) = &c.mount {
                        let lbl = format!("{:.2} MHz", c.freq / 1e6);
                        let t = now_playing(a.stations.get(&c.khz), if c.main { &label } else { &lbl });
                        if t != c.title {
                            hub.set_title(m, &t);
                            c.title = t;
                        }
                    }
                }
            }
        }
        for r in rds_results {
            merge(app, CaptureOutput { stations: vec![r], clip: 0.0, spectrum: vec![] }, true);
        }
        if last_save.elapsed() > Duration::from_secs(30) {
            save_stations(app, &cfg.save_path);
            last_save = Instant::now();
        }
    };

    if let Some(r) = recorder.take() {
        r.stop();
    }
    ice_stop.store(true, Ordering::Relaxed);
    for c in &chans {
        if let Some(m) = &c.mount {
            hub.close(m);
        }
    }
    {
        let mut a = app.lock().unwrap();
        a.rec_status = None;
        a.live.streams.clear();
    }
    stop.store(true, Ordering::Relaxed);
    drop(rx);
    let dev = reader.join().unwrap_or_else(|_| panic!("reader thread panicked"));
    let _ = enh_set0;
    (dev, result)
}

fn dev_ctrl_snapshot(app: &Arc<Mutex<App>>) -> (Option<f64>, String, bool) {
    let a = app.lock().unwrap();
    (if a.agc { None } else { Some(a.gain) }, a.antenna.clone(), a.bias_tee)
}

fn app_open_id(app: &Arc<Mutex<App>>) -> String {
    app.lock().unwrap().open_device.clone().unwrap_or_default()
}

fn pick_device(a: &mut App) {
    if a.selected_device().is_some_and(|d| d.present) {
        return;
    }
    // same kind as the saved choice, else any present device, else keep the default entry
    let kind = a.selected_device().map(|d| d.kind).unwrap_or(if a.device_id.starts_with("rtl") { Kind::RtlSdr } else { Kind::Uhd });
    let pick = a
        .devices
        .iter()
        .find(|d| d.present && d.kind == kind)
        .or_else(|| a.devices.iter().find(|d| d.present))
        .or_else(|| a.devices.iter().find(|d| d.kind == kind))
        .map(|d| d.id.clone());
    if let Some(id) = pick {
        a.device_id = id;
    }
}

pub fn run(app: Arc<Mutex<App>>, cfg: WorkerCfg) {
    let mut dsp = Dsp::new();
    let mut dev: Option<Open> = None;
    let mut chunk = 0usize;
    loop {
        // (re)discover devices
        if app.lock().unwrap().rescan_devices {
            dev = None;
            app.lock().unwrap().status = "Searching for devices…".into();
            let (list, notes) = device::enumerate(&cfg.paths);
            let mut a = app.lock().unwrap();
            a.devices = list;
            a.device_notes = notes;
            a.rescan_devices = false;
            a.open_device = None;
            pick_device(&mut a);
            let found: Vec<String> = a.devices.iter().filter(|d| d.present).map(|d| d.label.clone()).collect();
            a.push_log(if found.is_empty() { "No SDR found".to_string() } else { format!("Found: {}", found.join(", ")) });
        }
        let (mode, want_id, info, dirty) = {
            let a = app.lock().unwrap();
            (a.mode, a.device_id.clone(), a.selected_device().cloned(), a.settings_dirty)
        };
        if dirty {
            app.lock().unwrap().save_settings();
        }
        if mode == Mode::Idle {
            if dev.is_some() {
                dev = None;
                app.lock().unwrap().open_device = None;
            }
            app.lock().unwrap().status = "Stopped".into();
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        if dev.as_ref().map(|d| d.id != want_id).unwrap_or(true) {
            dev = None;
            let Some(info): Option<DeviceInfo> = info.clone() else {
                std::thread::sleep(Duration::from_millis(300));
                continue;
            };
            app.lock().unwrap().status = format!("Opening {}…", info.label);
            match Sdr::open(&info, &cfg.paths) {
                Ok(sdr) => {
                    let mut a = app.lock().unwrap();
                    a.error = None;
                    a.open_device = Some(info.id.clone());
                    a.push_log(format!("Opened {}", info.label));
                    dev = Some(Open { id: info.id.clone(), kind: info.kind, sdr, rate: 0.0, gain: None, antenna: None, bias: None });
                }
                Err(e) => {
                    let msg = hint(&e);
                    {
                        let mut a = app.lock().unwrap();
                        if a.error.as_deref() != Some(msg.as_str()) {
                            a.push_log(format!("Cannot open device: {msg}"));
                        }
                        a.error = Some(msg);
                        a.status = "Retrying in 3 seconds".into();
                    }
                    std::thread::sleep(Duration::from_secs(3));
                    continue;
                }
            }
        }
        let mut d = dev.take().unwrap();
        let res = match mode {
            Mode::Scan => {
                let r = d.apply_controls(&app).and_then(|_| scan_step(&mut d, &mut dsp, &app, &mut chunk, &cfg));
                dev = Some(d);
                r
            }
            Mode::Listen { hz, demod } => {
                let can = info.as_ref().map(|i| i.can_tune(hz as f64)).unwrap_or(true);
                if !can {
                    dev = Some(d);
                    let mut a = app.lock().unwrap();
                    let (lo, hi) = info.as_ref().unwrap().freq_range();
                    a.error = Some(format!(
                        "{} cannot tune {:.3} MHz (range {:.0}–{:.0} MHz). Use the RTL-SDR V4 for MW/SW.",
                        info.as_ref().unwrap().label,
                        hz as f64 / 1e6,
                        lo / 1e6,
                        hi / 1e6
                    ));
                    a.mode = Mode::Idle;
                    continue;
                }
                let (d2, r) = listen_session(d, &mut dsp, &app, hz, demod, &cfg);
                dev = Some(d2);
                r
            }
            Mode::Idle => {
                dev = Some(d);
                Ok(())
            }
        };
        match res {
            Ok(()) => {
                let mut a = app.lock().unwrap();
                if a.error.as_deref().is_some_and(|e| !e.contains("cannot tune")) {
                    a.error = None;
                }
            }
            Err(e) => {
                let recoverable = e.starts_with("overflow");
                {
                    let mut a = app.lock().unwrap();
                    a.push_log(format!("Error: {e}"));
                    if !recoverable {
                        a.error = Some(hint(&e));
                        a.open_device = None;
                    }
                }
                if !recoverable {
                    dev = None; // reopen the device
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}
