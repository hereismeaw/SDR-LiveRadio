//! RTL-SDR (incl. RTL-SDR Blog V4) through `rtlsdr.dll`, loaded at runtime.
//! The RTL-SDR Blog build of the driver switches the V4's HF upconverter in
//! automatically below 28.8 MHz, so MW / SW tuning works with plain set_center_freq.

use libloading::os::windows::{Library, LOAD_WITH_ALTERED_SEARCH_PATH};
use num_complex::Complex32 as C32;
use std::ffi::{c_char, c_void, CStr};
use std::path::Path;
use std::ptr::null_mut;

type Dev = *mut c_void;

struct Api {
    get_device_count: unsafe extern "C" fn() -> u32,
    get_device_usb_strings: unsafe extern "C" fn(u32, *mut c_char, *mut c_char, *mut c_char) -> i32,
    open: unsafe extern "C" fn(*mut Dev, u32) -> i32,
    close: unsafe extern "C" fn(Dev) -> i32,
    set_sample_rate: unsafe extern "C" fn(Dev, u32) -> i32,
    get_sample_rate: unsafe extern "C" fn(Dev) -> u32,
    set_center_freq: unsafe extern "C" fn(Dev, u32) -> i32,
    get_center_freq: unsafe extern "C" fn(Dev) -> u32,
    set_tuner_gain_mode: unsafe extern "C" fn(Dev, i32) -> i32,
    set_tuner_gain: unsafe extern "C" fn(Dev, i32) -> i32,
    get_tuner_gains: unsafe extern "C" fn(Dev, *mut i32) -> i32,
    set_agc_mode: unsafe extern "C" fn(Dev, i32) -> i32,
    set_freq_correction: unsafe extern "C" fn(Dev, i32) -> i32,
    set_bias_tee: unsafe extern "C" fn(Dev, i32) -> i32,
    reset_buffer: unsafe extern "C" fn(Dev) -> i32,
    read_sync: unsafe extern "C" fn(Dev, *mut c_void, i32, *mut i32) -> i32,
    _lib: Library,
}

macro_rules! sym {
    ($lib:ident, $name:literal) => {
        *unsafe { $lib.get(concat!($name, "\0").as_bytes()) }
            .map_err(|e| format!("function {} not found in rtlsdr.dll: {}", $name, e))?
    };
}

impl Api {
    fn load(path: &Path) -> Result<Self, String> {
        let lib = unsafe { Library::load_with_flags(path, LOAD_WITH_ALTERED_SEARCH_PATH) }
            .map_err(|e| format!("cannot load {}: {}", path.display(), e))?;
        Ok(Api {
            get_device_count: sym!(lib, "rtlsdr_get_device_count"),
            get_device_usb_strings: sym!(lib, "rtlsdr_get_device_usb_strings"),
            open: sym!(lib, "rtlsdr_open"),
            close: sym!(lib, "rtlsdr_close"),
            set_sample_rate: sym!(lib, "rtlsdr_set_sample_rate"),
            get_sample_rate: sym!(lib, "rtlsdr_get_sample_rate"),
            set_center_freq: sym!(lib, "rtlsdr_set_center_freq"),
            get_center_freq: sym!(lib, "rtlsdr_get_center_freq"),
            set_tuner_gain_mode: sym!(lib, "rtlsdr_set_tuner_gain_mode"),
            set_tuner_gain: sym!(lib, "rtlsdr_set_tuner_gain"),
            get_tuner_gains: sym!(lib, "rtlsdr_get_tuner_gains"),
            set_agc_mode: sym!(lib, "rtlsdr_set_agc_mode"),
            set_freq_correction: sym!(lib, "rtlsdr_set_freq_correction"),
            set_bias_tee: sym!(lib, "rtlsdr_set_bias_tee"),
            reset_buffer: sym!(lib, "rtlsdr_reset_buffer"),
            read_sync: sym!(lib, "rtlsdr_read_sync"),
            _lib: lib,
        })
    }
}

/// (index, manufacturer, product, serial) of every RTL-SDR the driver sees.
pub fn find_devices(dll: &Path) -> Result<Vec<(u32, String, String, String)>, String> {
    let api = Api::load(dll)?;
    let n = unsafe { (api.get_device_count)() };
    let mut out = Vec::new();
    for i in 0..n {
        let mut m = [0 as c_char; 256];
        let mut p = [0 as c_char; 256];
        let mut s = [0 as c_char; 256];
        unsafe { (api.get_device_usb_strings)(i, m.as_mut_ptr(), p.as_mut_ptr(), s.as_mut_ptr()) };
        let st = |b: &[c_char]| unsafe { CStr::from_ptr(b.as_ptr()) }.to_string_lossy().trim().to_string();
        out.push((i, st(&m), st(&p), st(&s)));
    }
    Ok(out)
}

pub struct RtlSdr {
    api: Api,
    dev: Dev,
    gains: Vec<i32>, // tenths of dB
    raw: Vec<u8>,
}

// The C handle is only ever used by one thread at a time.
unsafe impl Send for RtlSdr {}

fn check(rc: i32, what: &str) -> Result<(), String> {
    if rc < 0 {
        Err(format!("rtlsdr {what} failed ({rc})"))
    } else {
        Ok(())
    }
}

impl RtlSdr {
    pub fn open(dll: &Path, index: u32) -> Result<Self, String> {
        let api = Api::load(dll)?;
        let mut dev: Dev = null_mut();
        let rc = unsafe { (api.open)(&mut dev, index) };
        if rc < 0 || dev.is_null() {
            return Err(format!(
                "rtlsdr_open({index}) failed ({rc}) — is the dongle plugged in and using the WinUSB driver (Zadig)?"
            ));
        }
        let n = unsafe { (api.get_tuner_gains)(dev, null_mut()) }.max(0) as usize;
        let mut gains = vec![0i32; n];
        if n > 0 {
            unsafe { (api.get_tuner_gains)(dev, gains.as_mut_ptr()) };
        }
        unsafe {
            (api.set_agc_mode)(dev, 0);
            (api.set_freq_correction)(dev, 0);
        }
        Ok(Self { api, dev, gains, raw: Vec::new() })
    }

    pub fn gain_range(&self) -> (f64, f64) {
        match (self.gains.first(), self.gains.last()) {
            (Some(a), Some(b)) => (*a as f64 / 10.0, *b as f64 / 10.0),
            _ => (0.0, 49.6),
        }
    }

    pub fn set_rate(&mut self, rate: f64) -> Result<f64, String> {
        check(unsafe { (self.api.set_sample_rate)(self.dev, rate.round() as u32) }, "set_sample_rate")?;
        Ok(unsafe { (self.api.get_sample_rate)(self.dev) } as f64)
    }

    pub fn tune(&mut self, f: f64) -> Result<f64, String> {
        check(unsafe { (self.api.set_center_freq)(self.dev, f.round() as u32) }, "set_center_freq")?;
        Ok(unsafe { (self.api.get_center_freq)(self.dev) } as f64)
    }

    /// `None` = tuner AGC; otherwise the nearest supported gain.
    pub fn set_gain(&mut self, gain: Option<f64>) -> Result<(), String> {
        match gain {
            None => check(unsafe { (self.api.set_tuner_gain_mode)(self.dev, 0) }, "gain mode"),
            Some(g) => {
                check(unsafe { (self.api.set_tuner_gain_mode)(self.dev, 1) }, "gain mode")?;
                let target = (g * 10.0).round() as i32;
                let best = self.gains.iter().copied().min_by_key(|v| (v - target).abs()).unwrap_or(target);
                check(unsafe { (self.api.set_tuner_gain)(self.dev, best) }, "set_tuner_gain")
            }
        }
    }

    pub fn set_bias_tee(&mut self, on: bool) -> Result<(), String> {
        check(unsafe { (self.api.set_bias_tee)(self.dev, on as i32) }, "bias tee")
    }

    pub fn reset(&mut self) -> Result<(), String> {
        check(unsafe { (self.api.reset_buffer)(self.dev) }, "reset_buffer")
    }

    /// Blocking read of up to `out.len()` samples (multiple of 256 recommended).
    pub fn read(&mut self, out: &mut [C32]) -> Result<usize, String> {
        let bytes = (out.len() * 2) / 512 * 512;
        if bytes == 0 {
            return Ok(0);
        }
        self.raw.resize(bytes, 0);
        let mut n = 0i32;
        check(
            unsafe { (self.api.read_sync)(self.dev, self.raw.as_mut_ptr() as *mut c_void, bytes as i32, &mut n) },
            "read_sync",
        )?;
        let samples = n.max(0) as usize / 2;
        for (o, iq) in out.iter_mut().zip(self.raw.chunks_exact(2)).take(samples) {
            *o = C32::new((iq[0] as f32 - 127.4) / 128.0, (iq[1] as f32 - 127.4) / 128.0);
        }
        Ok(samples)
    }
}

impl Drop for RtlSdr {
    fn drop(&mut self) {
        if !self.dev.is_null() {
            unsafe { (self.api.close)(self.dev) };
        }
    }
}
