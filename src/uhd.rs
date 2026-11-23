//! Minimal runtime binding to the UHD C API (`uhd.dll`), loaded with libloading so the
//! program builds and self-tests without UHD. Struct layouts follow UHD 4.x `uhd/usrp/usrp.h`.

use libloading::os::windows::{Library, LOAD_WITH_ALTERED_SEARCH_PATH};
use num_complex::Complex32 as C32;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;
use std::ptr::null_mut;

type Handle = *mut c_void;
type UhdErr = i32;

#[repr(C)]
struct StreamArgs {
    cpu_format: *const c_char,
    otw_format: *const c_char,
    args: *const c_char,
    channel_list: *const usize,
    n_channels: i32,
}

#[repr(C)]
struct StreamCmd {
    stream_mode: i32,
    num_samps: usize,
    stream_now: bool,
    time_spec_full_secs: i64,
    time_spec_frac_secs: f64,
}

#[repr(C)]
struct TuneRequest {
    target_freq: f64,
    rf_freq_policy: i32,
    rf_freq: f64,
    dsp_freq_policy: i32,
    dsp_freq: f64,
    args: *const c_char,
}

#[repr(C)]
#[derive(Default)]
struct TuneResult {
    clipped_rf_freq: f64,
    target_rf_freq: f64,
    actual_rf_freq: f64,
    target_dsp_freq: f64,
    actual_dsp_freq: f64,
}

const STREAM_MODE_NUM_SAMPS_AND_DONE: i32 = 100;
const STREAM_MODE_START_CONTINUOUS: i32 = 97;
const STREAM_MODE_STOP_CONTINUOUS: i32 = 111;
const TUNE_POLICY_AUTO: i32 = 65;
const MD_ERROR_TIMEOUT: i32 = 0x1;
const MD_ERROR_OVERFLOW: i32 = 0x8;

struct Api {
    usrp_make: unsafe extern "C" fn(*mut Handle, *const c_char) -> UhdErr,
    usrp_free: unsafe extern "C" fn(*mut Handle) -> UhdErr,
    get_rx_num_channels: unsafe extern "C" fn(Handle, *mut usize) -> UhdErr,
    set_rx_rate: unsafe extern "C" fn(Handle, f64, usize) -> UhdErr,
    get_rx_rate: unsafe extern "C" fn(Handle, usize, *mut f64) -> UhdErr,
    set_rx_freq: unsafe extern "C" fn(Handle, *mut TuneRequest, usize, *mut TuneResult) -> UhdErr,
    get_rx_freq: unsafe extern "C" fn(Handle, usize, *mut f64) -> UhdErr,
    set_rx_gain: unsafe extern "C" fn(Handle, f64, usize, *const c_char) -> UhdErr,
    set_rx_antenna: unsafe extern "C" fn(Handle, *const c_char, usize) -> UhdErr,
    set_rx_bandwidth: unsafe extern "C" fn(Handle, f64, usize) -> UhdErr,
    get_rx_stream: unsafe extern "C" fn(Handle, *mut StreamArgs, Handle) -> UhdErr,
    streamer_make: unsafe extern "C" fn(*mut Handle) -> UhdErr,
    streamer_free: unsafe extern "C" fn(*mut Handle) -> UhdErr,
    streamer_max_num_samps: unsafe extern "C" fn(Handle, *mut usize) -> UhdErr,
    streamer_issue: unsafe extern "C" fn(Handle, *const StreamCmd) -> UhdErr,
    streamer_recv: unsafe extern "C" fn(Handle, *mut *mut c_void, usize, *mut Handle, f64, bool, *mut usize) -> UhdErr,
    md_make: unsafe extern "C" fn(*mut Handle) -> UhdErr,
    md_free: unsafe extern "C" fn(*mut Handle) -> UhdErr,
    md_error_code: unsafe extern "C" fn(Handle, *mut i32) -> UhdErr,
    get_last_error: unsafe extern "C" fn(*mut c_char, usize) -> UhdErr,
    usrp_find: unsafe extern "C" fn(*const c_char, *mut Handle) -> UhdErr,
    sv_make: unsafe extern "C" fn(*mut Handle) -> UhdErr,
    sv_free: unsafe extern "C" fn(*mut Handle) -> UhdErr,
    sv_size: unsafe extern "C" fn(Handle, *mut usize) -> UhdErr,
    sv_at: unsafe extern "C" fn(Handle, usize, *mut c_char, usize) -> UhdErr,
    _lib: Library,
}

macro_rules! sym {
    ($lib:ident, $name:literal) => {
        *unsafe { $lib.get(concat!($name, "\0").as_bytes()) }
            .map_err(|e| format!("function {} not found in uhd.dll: {}", $name, e))?
    };
}

impl Api {
    fn load(path: &Path) -> Result<Self, String> {
        let lib = unsafe { Library::load_with_flags(path, LOAD_WITH_ALTERED_SEARCH_PATH) }
            .map_err(|e| format!("cannot load {}: {}", path.display(), e))?;
        Ok(Api {
            usrp_make: sym!(lib, "uhd_usrp_make"),
            usrp_free: sym!(lib, "uhd_usrp_free"),
            get_rx_num_channels: sym!(lib, "uhd_usrp_get_rx_num_channels"),
            set_rx_rate: sym!(lib, "uhd_usrp_set_rx_rate"),
            get_rx_rate: sym!(lib, "uhd_usrp_get_rx_rate"),
            set_rx_freq: sym!(lib, "uhd_usrp_set_rx_freq"),
            get_rx_freq: sym!(lib, "uhd_usrp_get_rx_freq"),
            set_rx_gain: sym!(lib, "uhd_usrp_set_rx_gain"),
            set_rx_antenna: sym!(lib, "uhd_usrp_set_rx_antenna"),
            set_rx_bandwidth: sym!(lib, "uhd_usrp_set_rx_bandwidth"),
            get_rx_stream: sym!(lib, "uhd_usrp_get_rx_stream"),
            streamer_make: sym!(lib, "uhd_rx_streamer_make"),
            streamer_free: sym!(lib, "uhd_rx_streamer_free"),
            streamer_max_num_samps: sym!(lib, "uhd_rx_streamer_max_num_samps"),
            streamer_issue: sym!(lib, "uhd_rx_streamer_issue_stream_cmd"),
            streamer_recv: sym!(lib, "uhd_rx_streamer_recv"),
            md_make: sym!(lib, "uhd_rx_metadata_make"),
            md_free: sym!(lib, "uhd_rx_metadata_free"),
            md_error_code: sym!(lib, "uhd_rx_metadata_error_code"),
            get_last_error: sym!(lib, "uhd_get_last_error"),
            usrp_find: sym!(lib, "uhd_usrp_find"),
            sv_make: sym!(lib, "uhd_string_vector_make"),
            sv_free: sym!(lib, "uhd_string_vector_free"),
            sv_size: sym!(lib, "uhd_string_vector_size"),
            sv_at: sym!(lib, "uhd_string_vector_at"),
            _lib: lib,
        })
    }

    fn check(&self, code: UhdErr, what: &str) -> Result<(), String> {
        if code == 0 {
            return Ok(());
        }
        let mut buf = vec![0 as c_char; 4096];
        unsafe { (self.get_last_error)(buf.as_mut_ptr(), buf.len()) };
        let msg = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().trim().to_string();
        Err(format!("{what}: {msg} (UHD error {code})"))
    }
}

/// Device address strings (e.g. "type=b200,name=...,serial=...,product=B210") for `args`.
pub fn find_devices(dll: &Path, args: &str) -> Result<Vec<String>, String> {
    let api = Api::load(dll)?;
    let cargs = CString::new(args).map_err(|e| e.to_string())?;
    let mut sv: Handle = null_mut();
    api.check(unsafe { (api.sv_make)(&mut sv) }, "string_vector")?;
    let res = (|| {
        api.check(unsafe { (api.usrp_find)(cargs.as_ptr(), &mut sv) }, "find")?;
        let mut n = 0usize;
        api.check(unsafe { (api.sv_size)(sv, &mut n) }, "string_vector size")?;
        let mut out = Vec::new();
        for i in 0..n {
            let mut buf = vec![0 as c_char; 1024];
            api.check(unsafe { (api.sv_at)(sv, i, buf.as_mut_ptr(), buf.len()) }, "string_vector at")?;
            out.push(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned());
        }
        Ok(out)
    })();
    unsafe { (api.sv_free)(&mut sv) };
    res
}

pub struct Usrp {
    api: Api,
    usrp: Handle,
    streamer: Handle,
    md: Handle,
    channel: usize,
    max_samps: usize,
    pub overflows: u64,
}

// The C handles are only ever used by one thread at a time.
unsafe impl Send for Usrp {}

impl Usrp {
    /// Start continuous streaming (use `read` to receive, `stop_stream` to end).
    pub fn start_stream(&mut self) -> Result<(), String> {
        self.flush();
        let cmd = StreamCmd {
            stream_mode: STREAM_MODE_START_CONTINUOUS,
            num_samps: 0,
            stream_now: true,
            time_spec_full_secs: 0,
            time_spec_frac_secs: 0.0,
        };
        self.api.check(unsafe { (self.api.streamer_issue)(self.streamer, &cmd) }, "start stream")
    }

    pub fn stop_stream(&mut self) {
        let cmd = StreamCmd {
            stream_mode: STREAM_MODE_STOP_CONTINUOUS,
            num_samps: 0,
            stream_now: true,
            time_spec_full_secs: 0,
            time_spec_frac_secs: 0.0,
        };
        unsafe { (self.api.streamer_issue)(self.streamer, &cmd) };
        self.flush();
    }

    /// Receive up to `out.len()` samples of a continuous stream. Overflows are counted
    /// (the stream continues); a timeout is an error.
    pub fn read(&mut self, out: &mut [C32]) -> Result<usize, String> {
        let mut ptr = out.as_mut_ptr() as *mut c_void;
        let mut items = 0usize;
        self.api.check(
            unsafe { (self.api.streamer_recv)(self.streamer, &mut ptr, out.len(), &mut self.md, 1.0, false, &mut items) },
            "recv",
        )?;
        let mut code = 0i32;
        unsafe { (self.api.md_error_code)(self.md, &mut code) };
        match code {
            MD_ERROR_OVERFLOW => self.overflows += 1,
            MD_ERROR_TIMEOUT => return Err("timeout waiting for samples".into()),
            _ => {}
        }
        Ok(items)
    }

    pub fn open(dll: &Path, args: &str, channel: usize) -> Result<Self, String> {
        let api = Api::load(dll)?;
        let cargs = CString::new(args).map_err(|e| e.to_string())?;
        let mut usrp: Handle = null_mut();
        api.check(unsafe { (api.usrp_make)(&mut usrp, cargs.as_ptr()) }, "open USRP")?;
        let mut u = Usrp {
            api, usrp, channel, md: null_mut(), streamer: null_mut(), max_samps: 0, overflows: 0,
        };
        let mut count = 0;
        u.api.check(unsafe { (u.api.get_rx_num_channels)(u.usrp, &mut count) }, "RX channel count")?;
        if channel >= count {
            return Err(format!("RX channel {channel} is unavailable. UHD reports {count} RX channel(s)."));
        }
        u.api.check(unsafe { (u.api.md_make)(&mut u.md) }, "rx metadata")?;
        u.api.check(unsafe { (u.api.streamer_make)(&mut u.streamer) }, "rx streamer")?;
        u.setup_stream()?;
        Ok(u)
    }

    fn setup_stream(&mut self) -> Result<(), String> {
        let cpu = CString::new("fc32").unwrap();
        let otw = CString::new("sc16").unwrap();
        let args = CString::new("").unwrap();
        let chans = [self.channel];
        let mut sa = StreamArgs {
            cpu_format: cpu.as_ptr(),
            otw_format: otw.as_ptr(),
            args: args.as_ptr(),
            channel_list: chans.as_ptr(),
            n_channels: 1,
        };
        self.api.check(unsafe { (self.api.get_rx_stream)(self.usrp, &mut sa, self.streamer) }, "get_rx_stream")?;
        self.api.check(
            unsafe { (self.api.streamer_max_num_samps)(self.streamer, &mut self.max_samps) },
            "max_num_samps",
        )
    }

    /// Set the RX sample rate; returns the actual rate. Recreates the stream.
    pub fn set_rate(&mut self, rate: f64) -> Result<f64, String> {
        self.api.check(unsafe { (self.api.set_rx_rate)(self.usrp, rate, self.channel) }, "set_rx_rate")?;
        let mut actual = 0.0;
        self.api.check(unsafe { (self.api.get_rx_rate)(self.usrp, self.channel, &mut actual) }, "get_rx_rate")?;
        self.setup_stream()?;
        Ok(actual)
    }

    pub fn set_gain(&mut self, gain: f64) -> Result<(), String> {
        let name = CString::new("").unwrap();
        self.api.check(unsafe { (self.api.set_rx_gain)(self.usrp, gain, self.channel, name.as_ptr()) }, "set_rx_gain")
    }

    pub fn set_antenna(&mut self, ant: &str) -> Result<(), String> {
        let a = CString::new(ant).map_err(|e| e.to_string())?;
        self.api.check(unsafe { (self.api.set_rx_antenna)(self.usrp, a.as_ptr(), self.channel) }, "set_rx_antenna")
    }

    pub fn set_bandwidth(&mut self, bw: f64) -> Result<(), String> {
        self.api.check(unsafe { (self.api.set_rx_bandwidth)(self.usrp, bw, self.channel) }, "set_rx_bandwidth")
    }

    /// Tune RX; returns the actual center frequency.
    pub fn tune(&mut self, freq: f64) -> Result<f64, String> {
        let args = CString::new("").unwrap();
        let mut req = TuneRequest {
            target_freq: freq,
            rf_freq_policy: TUNE_POLICY_AUTO,
            rf_freq: 0.0,
            dsp_freq_policy: TUNE_POLICY_AUTO,
            dsp_freq: 0.0,
            args: args.as_ptr(),
        };
        let mut res = TuneResult::default();
        self.api.check(unsafe { (self.api.set_rx_freq)(self.usrp, &mut req, self.channel, &mut res) }, "set_rx_freq")?;
        let mut f = 0.0;
        self.api.check(unsafe { (self.api.get_rx_freq)(self.usrp, self.channel, &mut f) }, "get_rx_freq")?;
        Ok(f)
    }

    /// Drop any samples left in the transport from a previous burst.
    fn flush(&mut self) {
        let mut tmp = vec![C32::default(); self.max_samps.max(1)];
        for _ in 0..1000 {
            let mut ptr = tmp.as_mut_ptr() as *mut c_void;
            let mut items = 0usize;
            let rc = unsafe {
                (self.api.streamer_recv)(self.streamer, &mut ptr, tmp.len(), &mut self.md, 0.02, false, &mut items)
            };
            if rc != 0 || items == 0 {
                break;
            }
        }
    }

    /// Receive exactly `n` samples in one finite burst, discarding the first `discard`
    /// (LO / AGC settling after a retune).
    pub fn capture(&mut self, n: usize, discard: usize) -> Result<Vec<C32>, String> {
        self.flush();
        let total = n + discard;
        let mut buf = vec![C32::default(); total];
        let cmd = StreamCmd {
            stream_mode: STREAM_MODE_NUM_SAMPS_AND_DONE,
            num_samps: total,
            stream_now: true,
            time_spec_full_secs: 0,
            time_spec_frac_secs: 0.0,
        };
        self.api.check(unsafe { (self.api.streamer_issue)(self.streamer, &cmd) }, "issue_stream_cmd")?;
        let mut got = 0usize;
        let mut first = true;
        while got < total {
            let want = (total - got).min(self.max_samps.max(1) * 256);
            let mut ptr = unsafe { buf.as_mut_ptr().add(got) } as *mut c_void;
            let mut items = 0usize;
            let timeout = if first { 5.0 } else { 1.0 };
            self.api.check(
                unsafe { (self.api.streamer_recv)(self.streamer, &mut ptr, want, &mut self.md, timeout, false, &mut items) },
                "recv",
            )?;
            first = false;
            let mut code = 0i32;
            unsafe { (self.api.md_error_code)(self.md, &mut code) };
            match code {
                0 => {}
                MD_ERROR_TIMEOUT => return Err(format!("timeout: got {got}/{total} samples")),
                MD_ERROR_OVERFLOW => return Err("overflow: USB could not keep up (lower the sample rate)".into()),
                other if items == 0 => return Err(format!("rx metadata error 0x{other:X}")),
                _ => {}
            }
            got += items;
        }
        buf.drain(..discard);
        Ok(buf)
    }
}

impl Drop for Usrp {
    fn drop(&mut self) {
        unsafe {
            if !self.streamer.is_null() {
                (self.api.streamer_free)(&mut self.streamer);
            }
            if !self.md.is_null() {
                (self.api.md_free)(&mut self.md);
            }
            if !self.usrp.is_null() {
                (self.api.usrp_free)(&mut self.usrp);
            }
        }
    }
}
