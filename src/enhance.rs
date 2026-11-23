//! Audio enhancement at 48 kHz:
//! - Neural noise suppression: RNNoise (recurrent neural network, via `nnnoiseless`),
//!   mixed with the dry signal so music keeps its texture.
//! - Bandwidth extension: a harmonic exciter regenerates the octave above the
//!   broadcast cut-off (15 kHz on FM, the AM filter width on AM). This is DSP, not
//!   recovered information: it adds plausible "air", it cannot restore what was never sent.
//! - Stereo width (mid/side).

use crate::audio::Frame;
use nnnoiseless::DenoiseState;
use std::collections::VecDeque;
use std::f64::consts::TAU;

pub const RATE: f64 = 48_000.0;
const FRAME: usize = nnnoiseless::FRAME_SIZE; // 480 = 10 ms

#[derive(Clone, Debug, PartialEq)]
pub struct EnhanceSettings {
    pub denoise: bool,
    /// 0 = dry, 1 = fully denoised.
    pub denoise_mix: f32,
    pub bwe: bool,
    /// Level of the regenerated top octave (0..1).
    pub bwe_amount: f32,
    /// 1.0 = unchanged, 0 = mono, up to 2 = wide.
    pub width: f32,
}

impl Default for EnhanceSettings {
    fn default() -> Self {
        Self { denoise: false, denoise_mix: 0.7, bwe: false, bwe_amount: 0.35, width: 1.0 }
    }
}

#[derive(Clone, Copy, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn highpass(fs: f64, f0: f64, q: f64) -> Self {
        let w = TAU * f0 / fs;
        let alpha = w.sin() / (2.0 * q);
        let c = w.cos();
        let a0 = 1.0 + alpha;
        Self {
            b: [(1.0 + c) / 2.0 / a0, -(1.0 + c) / a0, (1.0 + c) / 2.0 / a0],
            a: [-2.0 * c / a0, (1.0 - alpha) / a0],
            z: [0.0; 2],
        }
    }
    fn lowpass(fs: f64, f0: f64, q: f64) -> Self {
        let w = TAU * f0 / fs;
        let alpha = w.sin() / (2.0 * q);
        let c = w.cos();
        let a0 = 1.0 + alpha;
        Self {
            b: [(1.0 - c) / 2.0 / a0, (1.0 - c) / a0, (1.0 - c) / 2.0 / a0],
            a: [-2.0 * c / a0, (1.0 - alpha) / a0],
            z: [0.0; 2],
        }
    }
    #[inline]
    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// Harmonic exciter for one channel.
struct Exciter {
    band: [Biquad; 2],  // isolate the octave below the cut-off
    top: [Biquad; 4],   // keep only what lies above the cut-off
    cutoff: f64,
}

impl Exciter {
    fn new(cutoff: f64) -> Self {
        let q = 1.0 / 2f64.sqrt();
        let lo = cutoff / 2.0;
        let top_edge = (cutoff * 1.02).min(RATE * 0.45);
        Self {
            band: [Biquad::highpass(RATE, lo, q), Biquad::lowpass(RATE, cutoff, q)],
            top: [
                Biquad::highpass(RATE, top_edge, q),
                Biquad::highpass(RATE, top_edge, q),
                Biquad::highpass(RATE, top_edge, q),
                Biquad::lowpass(RATE, (cutoff * 2.0).min(RATE * 0.47), q),
            ],
            cutoff,
        }
    }
    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let mut b = x as f64;
        for f in &mut self.band {
            b = f.run(b);
        }
        // full-wave rectification + soft clip -> even/odd harmonics an octave up
        let h = (b.abs() * 2.0 - 0.0).tanh() + 0.5 * (b * 3.0).tanh() * b.abs();
        let mut y = h;
        for f in &mut self.top {
            y = f.run(y);
        }
        y as f32
    }
}

pub struct Enhancer {
    dn: [Box<DenoiseState<'static>>; 2],
    pending: Vec<Frame>,
    wet: VecDeque<Frame>,
    dry: VecDeque<Frame>,
    exc: [Exciter; 2],
    tmp_in: Vec<f32>,
    tmp_out: Vec<f32>,
}

impl Enhancer {
    /// `cutoff_hz`: upper edge of the broadcast audio (15 kHz FM, AM filter width on AM).
    pub fn new(cutoff_hz: f64) -> Self {
        let mut e = Self {
            dn: [DenoiseState::new(), DenoiseState::new()],
            pending: Vec::new(),
            wet: VecDeque::new(),
            dry: VecDeque::new(),
            exc: [Exciter::new(cutoff_hz), Exciter::new(cutoff_hz)],
            tmp_in: vec![0.0; FRAME],
            tmp_out: vec![0.0; FRAME],
        };
        // constant 10 ms latency so wet and dry stay aligned
        e.wet.extend(std::iter::repeat([0.0; 2]).take(FRAME));
        e.dry.extend(std::iter::repeat([0.0; 2]).take(FRAME));
        e
    }

    pub fn set_cutoff(&mut self, cutoff_hz: f64) {
        if (self.exc[0].cutoff - cutoff_hz).abs() > 1.0 {
            self.exc = [Exciter::new(cutoff_hz), Exciter::new(cutoff_hz)];
        }
    }

    /// Process in place (48 kHz stereo). Output has a fixed 10 ms delay when denoising.
    pub fn process(&mut self, s: &EnhanceSettings, frames: &mut [Frame]) {
        if s.denoise {
            self.pending.extend_from_slice(frames);
            while self.pending.len() >= FRAME {
                let block: Vec<Frame> = self.pending.drain(..FRAME).collect();
                let mut out = vec![[0f32; 2]; FRAME];
                for ch in 0..2 {
                    for (i, f) in block.iter().enumerate() {
                        self.tmp_in[i] = f[ch] * 32768.0;
                    }
                    self.dn[ch].process_frame(&mut self.tmp_out, &self.tmp_in);
                    for (i, o) in out.iter_mut().enumerate() {
                        o[ch] = self.tmp_out[i] / 32768.0;
                    }
                }
                self.wet.extend(out);
                self.dry.extend(block);
            }
            let m = s.denoise_mix.clamp(0.0, 1.0);
            for f in frames.iter_mut() {
                let w = self.wet.pop_front().unwrap_or([0.0; 2]);
                let d = self.dry.pop_front().unwrap_or([0.0; 2]);
                *f = [m * w[0] + (1.0 - m) * d[0], m * w[1] + (1.0 - m) * d[1]];
            }
        }
        if s.bwe && s.bwe_amount > 0.0 {
            let k = s.bwe_amount * 0.5;
            for f in frames.iter_mut() {
                f[0] += k * self.exc[0].run(f[0]);
                f[1] += k * self.exc[1].run(f[1]);
            }
        }
        if (s.width - 1.0).abs() > 0.01 {
            for f in frames.iter_mut() {
                let m = 0.5 * (f[0] + f[1]);
                let sd = 0.5 * (f[0] - f[1]) * s.width;
                *f = [m + sd, m - sd];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn denoise_reduces_white_noise_and_keeps_length() {
        let mut e = Enhancer::new(15_000.0);
        let s = EnhanceSettings { denoise: true, denoise_mix: 1.0, ..Default::default() };
        let mut seed = 12345u64;
        let mut noise = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as f32 / (1u64 << 31) as f32 - 0.5) * 0.1
        };
        let mut x: Vec<Frame> = (0..48_000 * 2).map(|_| { let v = noise(); [v, v] }).collect();
        let before = rms(&x.iter().map(|f| f[0]).collect::<Vec<_>>());
        for c in x.chunks_mut(1000) {
            e.process(&s, c);
        }
        assert_eq!(x.len(), 96_000);
        let after = rms(&x[48_000..].iter().map(|f| f[0]).collect::<Vec<_>>());
        assert!(after < before * 0.5, "noise rms {before} -> {after}");
    }

    #[test]
    fn bandwidth_extension_adds_energy_above_cutoff_only() {
        let mut e = Enhancer::new(15_000.0);
        let s = EnhanceSettings { bwe: true, bwe_amount: 1.0, ..Default::default() };
        // 9 kHz tone (inside the band): harmonics at 18 kHz should appear
        let mut x: Vec<Frame> = (0..48_000).map(|i| {
            let v = (0.5 * (TAU * 9_000.0 * i as f64 / RATE).sin()) as f32;
            [v, v]
        }).collect();
        let orig = x.clone();
        e.process(&s, &mut x);
        let added: Vec<f32> = x.iter().zip(&orig).map(|(a, b)| a[0] - b[0]).collect();
        let tone = |f: f64| {
            let (mut re, mut im) = (0f64, 0f64);
            for (i, v) in added[10_000..].iter().enumerate() {
                let p = TAU * f * i as f64 / RATE;
                re += *v as f64 * p.cos();
                im += *v as f64 * p.sin();
            }
            2.0 * (re * re + im * im).sqrt() / (added.len() - 10_000) as f64
        };
        let (h18, f9) = (tone(18_000.0), tone(9_000.0));
        assert!(h18 > 0.003, "no 18 kHz harmonic: {h18}");
        assert!(f9 < h18 * 0.2, "exciter leaked into the band: 9k {f9} vs 18k {h18}");
    }
}
