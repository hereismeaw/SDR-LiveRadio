//! Audio post-processing and output: 10-band EQ, fractional resampler with drift
//! control, and a cpal (WASAPI) output stream.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::f64::consts::{PI, TAU};
use std::sync::{Arc, Mutex};

pub type Frame = [f32; 2];

// ---------------------------------------------------------------- equalizer

pub const EQ_FREQS: [f32; 10] = [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];

pub const EQ_PRESETS: [(&str, [f32; 10]); 10] = [
    ("Flat", [0.0; 10]),
    ("Bass boost", [6.0, 5.0, 4.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("Treble boost", [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 2.0, 4.0, 5.0, 6.0]),
    ("Loudness", [6.0, 4.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 4.0, 5.0]),
    ("Vocal", [-3.0, -2.0, -1.0, 1.0, 3.0, 4.0, 4.0, 2.0, 0.0, -1.0]),
    ("Pop", [-1.0, 1.0, 3.0, 4.0, 3.0, 0.0, -1.0, -1.0, 1.0, 2.0]),
    ("Rock", [5.0, 4.0, 2.0, -1.0, -2.0, -1.0, 1.0, 3.0, 4.0, 5.0]),
    ("Jazz", [3.0, 2.0, 1.0, 2.0, -1.0, -1.0, 0.0, 1.0, 2.0, 3.0]),
    ("Classical", [4.0, 3.0, 2.0, 1.0, -1.0, -1.0, 0.0, 2.0, 3.0, 4.0]),
    ("Speech / AM", [-12.0, -9.0, -4.0, 0.0, 2.0, 4.0, 4.0, 2.0, -3.0, -8.0]),
];

#[derive(Clone, Copy, Default)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [[f32; 2]; 2], // per channel
}

impl Biquad {
    /// RBJ cookbook peaking EQ.
    fn peaking(fs: f64, f0: f64, q: f64, gain_db: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w = TAU * f0 / fs;
        let alpha = w.sin() / (2.0 * q);
        let (b0, b1, b2) = (1.0 + alpha * a, -2.0 * w.cos(), 1.0 - alpha * a);
        let (a0, a1, a2) = (1.0 + alpha / a, -2.0 * w.cos(), 1.0 - alpha / a);
        Self {
            b: [(b0 / a0) as f32, (b1 / a0) as f32, (b2 / a0) as f32],
            a: [(a1 / a0) as f32, (a2 / a0) as f32],
            z: [[0.0; 2]; 2],
        }
    }

    #[inline]
    fn run(&mut self, ch: usize, x: f32) -> f32 {
        // transposed direct form II
        let z = &mut self.z[ch];
        let y = self.b[0] * x + z[0];
        z[0] = self.b[1] * x - self.a[0] * y + z[1];
        z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

pub struct Equalizer {
    fs: f64,
    gains: [f32; 10],
    bands: Vec<(usize, Biquad)>,
}

impl Equalizer {
    pub fn new(fs: f64) -> Self {
        Self { fs, gains: [0.0; 10], bands: Vec::new() }
    }

    /// Update gains (dB). Filters are rebuilt only when something changed.
    pub fn set(&mut self, gains: [f32; 10], enabled: bool) {
        let g = if enabled { gains } else { [0.0; 10] };
        if g == self.gains && (!self.bands.is_empty() || g.iter().all(|v| *v == 0.0)) {
            return;
        }
        self.gains = g;
        let old: Vec<(usize, Biquad)> = std::mem::take(&mut self.bands);
        for (i, &db) in g.iter().enumerate() {
            let f0 = EQ_FREQS[i] as f64;
            if db.abs() < 0.05 || f0 >= self.fs * 0.45 {
                continue;
            }
            let mut bq = Biquad::peaking(self.fs, f0, 1.41, db as f64);
            if let Some((_, prev)) = old.iter().find(|(j, _)| *j == i) {
                bq.z = prev.z; // keep state: no click when dragging a slider
            }
            self.bands.push((i, bq));
        }
    }

    pub fn process(&mut self, frames: &mut [Frame]) {
        if self.bands.is_empty() {
            return;
        }
        for f in frames.iter_mut() {
            for (_, bq) in self.bands.iter_mut() {
                f[0] = bq.run(0, f[0]);
                f[1] = bq.run(1, f[1]);
            }
        }
    }
}

// ---------------------------------------------------------------- resampler

/// Windowed-sinc polyphase resampler with an adjustable ratio (for clock drift).
pub struct Resampler {
    step: f64, // input samples per output sample
    adjust: f64,
    pos: f64,
    hist: Vec<Frame>,
    taps: usize,
    phases: usize,
    table: Vec<f32>,
    pub in_rate: f64,
    pub out_rate: f64,
}

impl Resampler {
    pub fn new(in_rate: f64, out_rate: f64) -> Self {
        let taps = 32;
        let phases = 256;
        let fc = 0.45 * (out_rate / in_rate).min(1.0); // cycles per input sample
        let half = taps as f64 / 2.0;
        let mut table = vec![0f32; phases * taps];
        for p in 0..phases {
            let frac = p as f64 / phases as f64;
            let mut row: Vec<f64> = (0..taps)
                .map(|k| {
                    let t = k as f64 - (half - 1.0) - frac;
                    let sinc = if t.abs() < 1e-12 { 2.0 * fc } else { (TAU * fc * t).sin() / (PI * t) };
                    let w = 0.5 + 0.5 * (PI * t / half).cos(); // Hann
                    sinc * if t.abs() < half { w } else { 0.0 }
                })
                .collect();
            let s: f64 = row.iter().sum();
            row.iter_mut().for_each(|v| *v /= s);
            for k in 0..taps {
                table[p * taps + k] = row[k] as f32;
            }
        }
        Self {
            step: in_rate / out_rate,
            adjust: 1.0,
            pos: (taps / 2 - 1) as f64,
            hist: vec![[0.0; 2]; taps],
            taps,
            phases,
            table,
            in_rate,
            out_rate,
        }
    }

    /// Multiply the ratio by `adj` (close to 1.0) to track a drifting output clock.
    pub fn set_adjust(&mut self, adj: f64) {
        self.adjust = adj;
    }

    pub fn process(&mut self, input: &[Frame], out: &mut Vec<Frame>) {
        self.hist.extend_from_slice(input);
        let half = self.taps / 2;
        let step = self.step * self.adjust;
        while self.pos + (half as f64) < self.hist.len() as f64 {
            let i = self.pos.floor() as usize;
            let frac = self.pos - i as f64;
            let p = ((frac * self.phases as f64) as usize).min(self.phases - 1);
            let row = &self.table[p * self.taps..(p + 1) * self.taps];
            let base = i + 1 - (half - 1) - 1;
            let mut acc = [0f32; 2];
            for (k, &h) in row.iter().enumerate() {
                let s = self.hist[base + k];
                acc[0] += s[0] * h;
                acc[1] += s[1] * h;
            }
            out.push(acc);
            self.pos += step;
        }
        let drop = (self.pos.floor() as usize).saturating_sub(self.taps);
        if drop > 0 {
            self.hist.drain(..drop);
            self.pos -= drop as f64;
        }
    }
}

// ---------------------------------------------------------------- output

pub fn output_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|it| it.filter_map(|d| d.description().ok().map(|x| x.name().to_string())).collect())
        .unwrap_or_default()
}

/// A running output stream fed from an internal FIFO (interleaved stereo).
pub struct AudioOut {
    _stream: cpal::Stream,
    fifo: Arc<Mutex<VecDeque<f32>>>,
    pub rate: u32,
    pub device_name: String,
}

impl AudioOut {
    /// Open `name` (or the default output device).
    pub fn open(name: Option<&str>) -> Result<Self, String> {
        let host = cpal::default_host();
        let dev = match name {
            Some(n) => host
                .output_devices()
                .map_err(|e| e.to_string())?
                .find(|d| d.description().map(|x| x.name() == n).unwrap_or(false))
                .ok_or_else(|| format!("audio device \"{n}\" not found"))?,
            None => host.default_output_device().ok_or("no default audio output device")?,
        };
        let device_name = dev.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "?".into());
        let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
        let channels = cfg.channels() as usize;
        let rate = cfg.sample_rate();
        let fifo: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::with_capacity(rate as usize)));
        let f2 = fifo.clone();
        let stream = dev
            .build_output_stream(
                cfg.config(),
                move |data: &mut [f32], _| {
                    let mut q = f2.lock().unwrap();
                    for frame in data.chunks_mut(channels) {
                        let l = q.pop_front().unwrap_or(0.0);
                        let r = q.pop_front().unwrap_or(0.0);
                        match frame.len() {
                            1 => frame[0] = 0.5 * (l + r),
                            _ => {
                                frame[0] = l;
                                frame[1] = r;
                                frame[2..].iter_mut().for_each(|v| *v = 0.0);
                            }
                        }
                    }
                },
                |e| eprintln!("audio stream error: {e}"),
                None,
            )
            .map_err(|e| format!("cannot open audio output: {e}"))?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(Self { _stream: stream, fifo, rate, device_name })
    }

    pub fn push(&self, frames: &[Frame]) {
        let mut q = self.fifo.lock().unwrap();
        for f in frames {
            q.push_back(f[0].clamp(-1.0, 1.0));
            q.push_back(f[1].clamp(-1.0, 1.0));
        }
        // never let latency run away (e.g. after a stall): keep at most 1 s
        let max = self.rate as usize * 2;
        while q.len() > max {
            q.pop_front();
        }
    }

    pub fn buffered_seconds(&self) -> f64 {
        self.fifo.lock().unwrap().len() as f64 / 2.0 / self.rate as f64
    }
}

/// Keeps the output FIFO near `target` seconds by nudging the resampler ratio.
pub fn drift_adjust(buffered: f64, target: f64) -> f64 {
    let err = (buffered - target) / target;
    1.0 + (err * 0.003).clamp(-0.005, 0.005)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(fs: f64, f: f64, n: usize) -> Vec<Frame> {
        (0..n).map(|i| {
            let v = (0.5 * (TAU * f * i as f64 / fs).sin()) as f32;
            [v, v]
        }).collect()
    }

    fn amp(x: &[Frame], f: f64, fs: f64) -> f64 {
        let (mut re, mut im) = (0f64, 0f64);
        for (i, v) in x.iter().enumerate() {
            let p = TAU * f * i as f64 / fs;
            re += v[0] as f64 * p.cos();
            im += v[0] as f64 * p.sin();
        }
        2.0 * (re * re + im * im).sqrt() / x.len() as f64
    }

    #[test]
    fn resampler_keeps_frequency_and_level() {
        let mut rs = Resampler::new(45_600.0, 48_000.0);
        let x = tone(45_600.0, 1000.0, 45_600);
        let mut out = Vec::new();
        for c in x.chunks(1000) {
            rs.process(c, &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() < 100, "len {}", out.len());
        let tail = &out[1000..47_000];
        let a = amp(tail, 1000.0, 48_000.0);
        assert!((a - 0.5).abs() < 0.01, "amplitude {a}");
        // residual after removing the tone must be small (THD+N)
        let a_off = amp(tail, 1100.0, 48_000.0);
        assert!(a_off < 0.005, "leak {a_off}");
    }

    #[test]
    fn eq_band_gain() {
        let fs = 45_600.0;
        for (band, gain) in [(5usize, 6.0f32), (3, -6.0)] {
            let mut eq = Equalizer::new(fs);
            let mut g = [0.0; 10];
            g[band] = gain;
            eq.set(g, true);
            let f = EQ_FREQS[band] as f64;
            let mut x = tone(fs, f, 45_600);
            eq.process(&mut x);
            let a = amp(&x[10_000..], f, fs);
            let db = 20.0 * (a / 0.5).log10();
            assert!((db - gain as f64).abs() < 0.5, "band {band}: {db:.2} dB, want {gain}");
        }
    }
}
