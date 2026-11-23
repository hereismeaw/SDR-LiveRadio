//! Audio demodulators: wideband FM with stereo (pilot PLL) and AM envelope.

use num_complex::Complex32 as C32;
use std::f64::consts::TAU;

/// Windowed-sinc (Blackman) low-pass taps, unity DC gain.
pub fn lowpass_taps(n: usize, cutoff_hz: f64, fs: f64) -> Vec<f32> {
    let fc = cutoff_hz / fs;
    let m = (n - 1) as f64 / 2.0;
    let mut h: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 - m;
            let sinc = if t == 0.0 { 2.0 * fc } else { (TAU * fc * t).sin() / (std::f64::consts::PI * t) };
            let w = 0.42 - 0.5 * (TAU * i as f64 / (n - 1) as f64).cos() + 0.08 * (2.0 * TAU * i as f64 / (n - 1) as f64).cos();
            sinc * w
        })
        .collect();
    let s: f64 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= s);
    h.into_iter().map(|v| v as f32).collect()
}

/// FIR low-pass + decimate by `d`, streaming.
pub struct FirDecim {
    taps: Vec<f32>,
    hist: Vec<f32>, // doubled ring buffer
    pos: usize,
    d: usize,
    phase: usize,
}

impl FirDecim {
    pub fn new(taps: Vec<f32>, d: usize) -> Self {
        let n = taps.len();
        Self { taps, hist: vec![0.0; 2 * n], pos: 0, d, phase: 0 }
    }

    #[inline]
    pub fn push(&mut self, x: f32) -> Option<f32> {
        let n = self.taps.len();
        self.hist[self.pos] = x;
        self.hist[self.pos + n] = x;
        self.pos = (self.pos + 1) % n;
        self.phase += 1;
        if self.phase < self.d {
            return None;
        }
        self.phase = 0;
        // hist[pos .. pos+n] is oldest..newest
        let w = &self.hist[self.pos..self.pos + n];
        Some(w.iter().zip(self.taps.iter().rev()).map(|(a, b)| a * b).sum())
    }
}

/// Quadrature discriminator, continuous across calls. Output in rad/sample.
#[derive(Default)]
pub struct FmDiscriminator {
    prev: C32,
}

impl FmDiscriminator {
    pub fn process(&mut self, x: &[C32], out: &mut Vec<f32>) {
        for &v in x {
            out.push((v * self.prev.conj()).arg());
            self.prev = v;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StereoMode {
    /// Stereo whenever the pilot is locked (default).
    Auto,
    Mono,
}

/// FM MPX (228 kHz) -> L/R audio at 45.6 kHz.
pub struct StereoDecoder {
    fs: f64,
    phase: f64,
    w: f64,
    w0: f64,
    amp: f32,
    kp: f64,
    ki: f64,
    fir_m: FirDecim,
    fir_s: FirDecim,
    blend: f32,
    de_l: f32,
    de_r: f32,
    pub pilot_locked: bool,
    pub pilot_level: f32,
}

/// 75 kHz deviation in rad/sample at 228 kHz.
const FULL_DEV: f32 = (TAU * 75_000.0 / 228_000.0) as f32;

impl StereoDecoder {
    pub const OUT_RATE: f64 = 45_600.0;

    pub fn new() -> Self {
        let fs = 228_000.0;
        let wn = TAU * 15.0 / fs; // PLL natural frequency 15 Hz
        let zeta = 0.707;
        let taps = lowpass_taps(255, 15_000.0, fs);
        Self {
            fs,
            phase: 0.0,
            w: TAU * 19_000.0 / fs,
            w0: TAU * 19_000.0 / fs,
            amp: 0.0,
            kp: 2.0 * zeta * wn,
            ki: wn * wn,
            fir_m: FirDecim::new(taps.clone(), 5),
            fir_s: FirDecim::new(taps, 5),
            blend: 0.0,
            de_l: 0.0,
            de_r: 0.0,
            pilot_locked: false,
            pilot_level: 0.0,
        }
    }

    /// `deemph_us`: 50 (Thailand, Europe, Asia), 75 (Americas, Korea) or None.
    pub fn process(&mut self, mpx: &[f32], mode: StereoMode, deemph_us: Option<f32>, out: &mut Vec<[f32; 2]>) {
        let a = deemph_us.map(|us| 1.0 - (-1.0 / (Self::OUT_RATE as f32 * us * 1e-6)).exp());
        let max_dw = TAU * 50.0 / self.fs;
        for &x in mpx {
            let (s, c) = self.phase.sin_cos();
            let (s, c) = (s as f32, c as f32);
            // pilot PLL: cos(phase) tracks the pilot
            self.amp += 0.0002 * (x * c - self.amp);
            let err = (x * -s) as f64 / (self.amp.abs() as f64 * 2.0 + 0.02);
            self.w = (self.w + self.ki * err).clamp(self.w0 - max_dw, self.w0 + max_dw);
            self.phase += self.w + self.kp * err;
            if self.phase > TAU {
                self.phase -= TAU;
            }
            // L-R on the 38 kHz subcarrier: 2*LPF(mpx * sin(2wt)) = -4 x s c (see derivation in docs)
            let sub = -4.0 * x * s * c;
            let m = self.fir_m.push(x);
            let sd = self.fir_s.push(sub);
            if let (Some(m), Some(sd)) = (m, sd) {
                // pilot in-phase amplitude ~ A/2; 9% pilot at 75 kHz deviation => A ~ 0.186 rad/sample
                self.pilot_level = 2.0 * self.amp / FULL_DEV;
                let lvl = self.pilot_level;
                self.pilot_locked = if self.pilot_locked { lvl > 0.025 } else { lvl > 0.04 };
                let target = if self.pilot_locked && mode == StereoMode::Auto { 1.0 } else { 0.0 };
                self.blend += 0.002 * (target - self.blend);
                let (mut l, mut r) = ((m + sd * self.blend) / FULL_DEV, (m - sd * self.blend) / FULL_DEV);
                if let Some(a) = a {
                    self.de_l += a * (l - self.de_l);
                    self.de_r += a * (r - self.de_r);
                    l = self.de_l;
                    r = self.de_r;
                }
                out.push([l, r]);
            }
        }
    }
}

impl Default for StereoDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// AM envelope detector with carrier-referenced AGC.
pub struct AmDemod {
    carrier: f32,
    hp: f32,
}

impl AmDemod {
    pub fn new() -> Self {
        Self { carrier: 0.0, hp: 0.0 }
    }

    pub fn process(&mut self, z: &[C32], out: &mut Vec<[f32; 2]>) {
        for v in z {
            let e = v.norm();
            if self.carrier == 0.0 {
                self.carrier = e;
            }
            self.carrier += 0.0003 * (e - self.carrier);
            let a = (e - self.carrier) / self.carrier.max(1e-9);
            self.hp += 0.002 * (a - self.hp); // remove residual DC / sub-audible
            let y = ((a - self.hp) * 0.8).clamp(-1.5, 1.5);
            out.push([y, y]);
        }
    }
}

impl Default for AmDemod {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone_amp(x: &[f32], f: f64, fs: f64) -> f32 {
        let (mut re, mut im) = (0f64, 0f64);
        for (i, &v) in x.iter().enumerate() {
            let p = TAU * f * i as f64 / fs;
            re += v as f64 * p.cos();
            im += v as f64 * p.sin();
        }
        (2.0 * (re * re + im * im).sqrt() / x.len() as f64) as f32
    }

    /// MPX with L = 1 kHz tone, R = 3 kHz tone, pilot sin(wt), L-R on sin(2wt).
    fn stereo_mpx(n: usize) -> Vec<f32> {
        let fs = 228_000.0;
        (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let l = 0.4 * (TAU * 1000.0 * t).sin();
                let r = 0.4 * (TAU * 3000.0 * t).sin();
                let mpx = (l + r) / 2.0 + 0.09 * (TAU * 19_000.0 * t).sin() + (l - r) / 2.0 * (TAU * 38_000.0 * t).sin();
                (mpx * FULL_DEV as f64) as f32
            })
            .collect()
    }

    #[test]
    fn stereo_separation() {
        let mpx = stereo_mpx(228_000 * 3);
        let mut dec = StereoDecoder::new();
        let mut out = Vec::new();
        dec.process(&mpx, StereoMode::Auto, None, &mut out);
        assert!(dec.pilot_locked, "pilot level {}", dec.pilot_level);
        let tail = &out[out.len() / 2..]; // after PLL lock and blend ramp
        let l: Vec<f32> = tail.iter().map(|s| s[0]).collect();
        let r: Vec<f32> = tail.iter().map(|s| s[1]).collect();
        let fs = StereoDecoder::OUT_RATE;
        let (l1, l3, r1, r3) = (tone_amp(&l, 1000.0, fs), tone_amp(&l, 3000.0, fs), tone_amp(&r, 1000.0, fs), tone_amp(&r, 3000.0, fs));
        let sep_l = 20.0 * (l1 / l3.max(1e-9)).log10();
        let sep_r = 20.0 * (r3 / r1.max(1e-9)).log10();
        assert!((l1 - 0.4).abs() < 0.04 && (r3 - 0.4).abs() < 0.04, "levels L1 {l1} R3 {r3}");
        assert!(sep_l > 30.0 && sep_r > 30.0, "separation L {sep_l:.1} dB R {sep_r:.1} dB");
        // forced mono: both channels carry both tones equally
        let mut out = Vec::new();
        let mut dec = StereoDecoder::new();
        dec.process(&mpx, StereoMode::Mono, None, &mut out);
        let r: Vec<f32> = out[out.len() / 2..].iter().map(|s| s[1]).collect();
        assert!((tone_amp(&r, 1000.0, fs) - 0.2).abs() < 0.03);
    }

    #[test]
    fn am_envelope() {
        let fs = 47_500.0;
        let z: Vec<C32> = (0..(fs as usize * 2))
            .map(|i| {
                let m = 1.0 + 0.5 * (TAU * 800.0 * i as f64 / fs).sin();
                C32::from_polar((0.01 * m) as f32, (i as f32) * 0.3) // carrier phase rotating
            })
            .collect();
        let mut am = AmDemod::new();
        let mut out = Vec::new();
        am.process(&z, &mut out);
        let x: Vec<f32> = out[out.len() / 2..].iter().map(|s| s[0]).collect();
        let a = tone_amp(&x, 800.0, fs);
        assert!((a - 0.4).abs() < 0.05, "AM audio amplitude {a}");
    }
}
