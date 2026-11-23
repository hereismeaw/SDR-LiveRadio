//! SDR device abstraction over UHD (USRP B2xx / LibreSDR) and RTL-SDR (incl. Blog V4).

use crate::rtlsdr::{self, RtlSdr};
use crate::uhd::{self, Usrp};
use num_complex::Complex32 as C32;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Uhd,
    RtlSdr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    pub kind: Kind,
    /// Stable key for settings, e.g. "uhd:B210A1C" or "rtl:00000001".
    pub id: String,
    pub label: String,
    /// UHD device args, or the RTL-SDR index.
    pub open_arg: String,
    pub present: bool,
}

impl DeviceInfo {
    /// Tunable range in Hz.
    pub fn freq_range(&self) -> (f64, f64) {
        match self.kind {
            Kind::Uhd => (50e6, 6e9),
            Kind::RtlSdr => (500e3, 1766e6),
        }
    }
    pub fn can_tune(&self, f: f64) -> bool {
        let (lo, hi) = self.freq_range();
        f >= lo && f <= hi
    }
}

#[derive(Clone, Debug)]
pub struct DriverPaths {
    pub uhd_dll: PathBuf,
    pub rtl_dll: PathBuf,
}

impl Default for DriverPaths {
    fn default() -> Self {
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
        let rtl = exe_dir
            .map(|d| d.join("rtlsdr.dll"))
            .filter(|p| p.exists())
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files\SDRangel\rtlsdr.dll"));
        Self {
            uhd_dll: PathBuf::from(std::env::var("UHD_DLL").unwrap_or_else(|_| r"C:\Program Files\UHD\bin\uhd.dll".into())),
            rtl_dll: PathBuf::from(std::env::var("RTLSDR_DLL").map(PathBuf::from).unwrap_or(rtl)),
        }
    }
}

fn addr_field<'a>(addr: &'a str, key: &str) -> Option<&'a str> {
    addr.split(',').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

/// Discover devices. The two default entries (LibreSDR B210mini, RTL-SDR Blog V4) are
/// always listed so they can be selected; `present` tells whether they were found.
pub fn enumerate(paths: &DriverPaths) -> (Vec<DeviceInfo>, Vec<String>) {
    let mut out = Vec::new();
    let mut notes = Vec::new();
    match uhd::find_devices(&paths.uhd_dll, "type=b200") {
        Ok(found) => {
            for a in found {
                let serial = addr_field(&a, "serial").unwrap_or("?");
                let name = addr_field(&a, "name").unwrap_or("USRP");
                let product = addr_field(&a, "product").unwrap_or("B2xx");
                out.push(DeviceInfo {
                    kind: Kind::Uhd,
                    id: format!("uhd:{serial}"),
                    label: format!("{name} ({product}, UHD) #{serial}"),
                    open_arg: format!("type=b200,serial={serial}"),
                    present: true,
                });
            }
        }
        Err(e) => notes.push(format!("UHD: {e}")),
    }
    match rtlsdr::find_devices(&paths.rtl_dll) {
        Ok(found) => {
            for (i, m, p, s) in found {
                out.push(DeviceInfo {
                    kind: Kind::RtlSdr,
                    id: format!("rtl:{s}"),
                    label: format!("{} {} #{s}", m, p).trim().to_string(),
                    open_arg: i.to_string(),
                    present: true,
                });
            }
        }
        Err(e) => notes.push(format!("RTL-SDR: {e}")),
    }
    if !out.iter().any(|d| d.kind == Kind::Uhd) {
        out.push(DeviceInfo {
            kind: Kind::Uhd,
            id: "uhd:default".into(),
            label: "LibreSDR B210mini (UHD) — not found".into(),
            open_arg: "type=b200".into(),
            present: false,
        });
    }
    if !out.iter().any(|d| d.kind == Kind::RtlSdr) {
        out.push(DeviceInfo {
            kind: Kind::RtlSdr,
            id: "rtl:default".into(),
            label: "RTL-SDR Blog V4 — not found".into(),
            open_arg: "0".into(),
            present: false,
        });
    }
    (out, notes)
}

pub enum Sdr {
    Uhd(Usrp),
    Rtl(RtlSdr),
}

impl Sdr {
    pub fn open(info: &DeviceInfo, paths: &DriverPaths) -> Result<Self, String> {
        match info.kind {
            Kind::Uhd => Usrp::open(&paths.uhd_dll, &info.open_arg).map(Sdr::Uhd),
            Kind::RtlSdr => {
                let idx = info.open_arg.parse().unwrap_or(0);
                RtlSdr::open(&paths.rtl_dll, idx).map(Sdr::Rtl)
            }
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Sdr::Uhd(_) => Kind::Uhd,
            Sdr::Rtl(_) => Kind::RtlSdr,
        }
    }

    pub fn gain_range(&self) -> (f64, f64) {
        match self {
            Sdr::Uhd(_) => (0.0, 76.0),
            Sdr::Rtl(r) => r.gain_range(),
        }
    }

    pub fn set_rate(&mut self, rate: f64) -> Result<f64, String> {
        match self {
            Sdr::Uhd(u) => {
                let got = u.set_rate(rate)?;
                u.set_bandwidth((rate * 0.9).max(200e3))?;
                Ok(got)
            }
            Sdr::Rtl(r) => r.set_rate(rate),
        }
    }

    pub fn tune(&mut self, f: f64) -> Result<f64, String> {
        match self {
            Sdr::Uhd(u) => u.tune(f),
            Sdr::Rtl(r) => r.tune(f),
        }
    }

    /// `None` = automatic gain (RTL tuner AGC; UHD has none here, uses mid gain).
    pub fn set_gain(&mut self, gain: Option<f64>) -> Result<(), String> {
        match self {
            Sdr::Uhd(u) => u.set_gain(gain.unwrap_or(40.0)),
            Sdr::Rtl(r) => r.set_gain(gain),
        }
    }

    pub fn set_antenna(&mut self, ant: &str) -> Result<(), String> {
        match self {
            Sdr::Uhd(u) => u.set_antenna(ant),
            Sdr::Rtl(_) => Ok(()),
        }
    }

    pub fn set_bias_tee(&mut self, on: bool) -> Result<(), String> {
        match self {
            Sdr::Uhd(_) => Ok(()),
            Sdr::Rtl(r) => r.set_bias_tee(on),
        }
    }

    /// One finite burst of `n` samples after dropping `discard` settling samples.
    pub fn capture(&mut self, n: usize, discard: usize) -> Result<Vec<C32>, String> {
        match self {
            Sdr::Uhd(u) => u.capture(n, discard),
            Sdr::Rtl(r) => {
                r.reset()?;
                let total = n + discard;
                let mut buf = vec![C32::default(); total];
                let mut got = 0;
                while got < total {
                    let chunk = (total - got).min(1 << 17);
                    let k = r.read(&mut buf[got..got + chunk])?;
                    if k == 0 {
                        return Err("RTL-SDR returned no samples".into());
                    }
                    got += k;
                }
                buf.drain(..discard);
                Ok(buf)
            }
        }
    }

    pub fn start_stream(&mut self) -> Result<(), String> {
        match self {
            Sdr::Uhd(u) => u.start_stream(),
            Sdr::Rtl(r) => r.reset(),
        }
    }

    pub fn read(&mut self, out: &mut [C32]) -> Result<usize, String> {
        match self {
            Sdr::Uhd(u) => u.read(out),
            Sdr::Rtl(r) => r.read(out),
        }
    }

    pub fn stop_stream(&mut self) {
        if let Sdr::Uhd(u) = self {
            u.stop_stream();
        }
    }

    /// Overflow count (UHD only; RTL-SDR does not report drops).
    pub fn overflows(&self) -> u64 {
        match self {
            Sdr::Uhd(u) => u.overflows,
            Sdr::Rtl(_) => 0,
        }
    }
}
