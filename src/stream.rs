//! Streaming FFT channelizer (overlap-save): shifts channels to 0 Hz, band-limits them
//! with a raised-cosine taper and decimates by `d`, block by block, phase-continuous.
//! Several channels share one forward FFT per block (multi-station reception).

use crate::dsp::{taper, Dsp};
use num_complex::Complex32 as C32;

struct Chan {
    k0: i64,
    weights: Vec<(usize, usize, f32)>, // (source bin, output bin, weight)
}

pub struct Channelizer {
    nfft: usize,
    d: usize,
    ov: usize,
    hop: usize,
    m: usize,
    chans: Vec<Chan>,
    buf: Vec<C32>,
    start: u64, // absolute input index of buf[0]
    scratch: Vec<C32>,
    y: Vec<C32>,
    pub in_rate: f64,
    pub out_rate: f64,
    want_spectrum: bool,
    spectrum: Option<Vec<f32>>,
}

impl Channelizer {
    /// One channel. `m_out`: output samples per FFT block (power of two); `ov_out`:
    /// output samples discarded at each block edge (longer than the filter response).
    pub fn new(fs: f64, d: usize, m_out: usize, ov_out: usize, offset_hz: f64, pass: f64, stop: f64) -> Self {
        Self::new_multi(fs, d, m_out, ov_out, &[(offset_hz, pass, stop)])
    }

    /// Several channels `(offset Hz, pass Hz, stop Hz)` with the same decimation.
    pub fn new_multi(fs: f64, d: usize, m_out: usize, ov_out: usize, chans: &[(f64, f64, f64)]) -> Self {
        let nfft = d * m_out;
        let ov = d * ov_out;
        let mut c = Self {
            nfft,
            d,
            ov,
            hop: nfft - 2 * ov,
            m: m_out,
            chans: Vec::new(),
            buf: Vec::with_capacity(2 * nfft),
            start: 0,
            scratch: vec![C32::default(); nfft],
            y: vec![C32::default(); m_out],
            in_rate: fs,
            out_rate: fs / d as f64,
            want_spectrum: false,
            spectrum: None,
        };
        for _ in chans {
            c.chans.push(Chan { k0: 0, weights: Vec::new() });
        }
        for (i, &(o, p, s)) in chans.iter().enumerate() {
            c.set_filter(i, o, p, s);
        }
        c
    }

    pub fn channels(&self) -> usize {
        self.chans.len()
    }

    /// Change a channel's offset or bandwidth without dropping buffered input.
    pub fn set_filter(&mut self, ch: usize, offset_hz: f64, pass: f64, stop: f64) {
        let bin = self.in_rate / self.nfft as f64;
        let k0 = (offset_hz / bin).round() as i64;
        let n = self.nfft as i64;
        let half = (self.m / 2) as i64;
        let mut weights = Vec::new();
        for j in 0..self.m {
            let rel = if (j as i64) < half { j as i64 } else { j as i64 - self.m as i64 };
            let w = taper((rel as f64 * bin).abs(), pass, stop);
            if w > 0.0 {
                weights.push(((k0 + rel).rem_euclid(n) as usize, j, w));
            }
        }
        self.chans[ch] = Chan { k0, weights };
    }

    /// Ask for a power spectrum (dBFS, FFT-shifted, 1024 bins) of the next block.
    pub fn request_spectrum(&mut self) {
        self.want_spectrum = true;
    }

    pub fn take_spectrum(&mut self) -> Option<Vec<f32>> {
        self.spectrum.take()
    }

    /// Single-channel convenience.
    pub fn push(&mut self, dsp: &mut Dsp, input: &[C32], out: &mut Vec<C32>) {
        self.push_multi(dsp, input, std::slice::from_mut(out));
    }

    /// Append each channel's output to `outs[i]`.
    pub fn push_multi(&mut self, dsp: &mut Dsp, input: &[C32], outs: &mut [Vec<C32>]) {
        self.buf.extend_from_slice(input);
        let scale = 1.0 / self.nfft as f32;
        let (keep_lo, keep_hi) = (self.ov / self.d, (self.nfft - self.ov) / self.d);
        while self.buf.len() >= self.nfft {
            self.scratch.copy_from_slice(&self.buf[..self.nfft]);
            dsp.fft(&mut self.scratch);
            if self.want_spectrum {
                self.want_spectrum = false;
                self.spectrum = Some(power_spectrum(&self.scratch, 1024));
            }
            for (ci, ch) in self.chans.iter().enumerate() {
                self.y.iter_mut().for_each(|v| *v = C32::default());
                for &(src, j, w) in &ch.weights {
                    self.y[j] = self.scratch[src] * w;
                }
                dsp.ifft(&mut self.y);
                // continuous mixing phase: exp(-j 2pi k0 start / nfft)
                let cyc = ((ch.k0 as i128 * self.start as i128).rem_euclid(self.nfft as i128)) as f64 / self.nfft as f64;
                let rot = C32::from_polar(scale, (-std::f64::consts::TAU * cyc) as f32);
                if let Some(o) = outs.get_mut(ci) {
                    o.extend(self.y[keep_lo..keep_hi].iter().map(|v| v * rot));
                }
            }
            self.buf.drain(..self.hop);
            self.start += self.hop as u64;
        }
    }
}

/// FFT-shifted power spectrum in dBFS averaged into `points` bins.
fn power_spectrum(spec: &[C32], points: usize) -> Vec<f32> {
    let n = spec.len();
    let per = (n / points).max(1);
    let nf = n as f64;
    (0..points)
        .map(|p| {
            let s: f64 = (0..per)
                .map(|i| {
                    let k = (p * per + i + n / 2) % n;
                    spec[k].norm_sqr() as f64
                })
                .sum();
            (10.0 * (s / per as f64 / (nf * nf) + 1e-30).log10()) as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(fs: f64, f: f64, n: usize, a: f32) -> Vec<C32> {
        (0..n).map(|i| C32::from_polar(a, ((f * i as f64 / fs).rem_euclid(1.0) * std::f64::consts::TAU) as f32)).collect()
    }

    #[test]
    fn tone_is_shifted_and_phase_continuous_across_blocks() {
        // 1.14 MS/s, tone at -250 kHz + 1 kHz; channel centred at -250 kHz
        let fs = 1_140_000.0;
        let x = tone(fs, -250_000.0 + 1_000.0, 600_000, 0.5);
        let mut dsp = Dsp::new();
        let mut ch = Channelizer::new(fs, 5, 1 << 15, 1 << 11, -250_000.0, 100e3, 114e3);
        let mut out = Vec::new();
        for chunk in x.chunks(65_536) {
            ch.push(&mut dsp, chunk, &mut out);
        }
        assert!(out.len() > 50_000);
        // instantaneous frequency must be +1 kHz everywhere, incl. block joins
        // (a phase discontinuity at a join would show up as an error of ~radians)
        let want = std::f32::consts::TAU * 1_000.0 / 228_000.0;
        let worst = out.windows(2).map(|w| ((w[1] * w[0].conj()).arg() - want).abs()).fold(0f32, f32::max);
        assert!(worst < 1e-3, "phase error {worst}");
        let amp = out.iter().map(|v| v.norm()).sum::<f32>() / out.len() as f32;
        assert!((amp - 0.5).abs() < 0.01, "amplitude {amp}");
    }

    #[test]
    fn two_channels_from_one_fft_are_isolated() {
        // 4.56 MS/s with stations at -1.5 MHz (+2 kHz tone) and +0.8 MHz (+5 kHz tone)
        let fs = 4_560_000.0;
        let n = 1_500_000;
        let a = tone(fs, -1.5e6 + 2_000.0, n, 0.3);
        let b = tone(fs, 0.8e6 + 5_000.0, n, 0.1);
        let x: Vec<C32> = a.iter().zip(&b).map(|(p, q)| p + q).collect();
        let mut dsp = Dsp::new();
        let mut ch = Channelizer::new_multi(fs, 20, 1 << 15, 1 << 11, &[(-1.5e6, 100e3, 114e3), (0.8e6, 100e3, 114e3)]);
        let mut outs = vec![Vec::new(), Vec::new()];
        for c in x.chunks(100_000) {
            ch.push_multi(&mut dsp, c, &mut outs);
        }
        for (o, f, amp) in [(&outs[0], 2_000.0f32, 0.3f32), (&outs[1], 5_000.0, 0.1)] {
            assert!(o.len() > 40_000);
            let want = std::f32::consts::TAU * f / 228_000.0;
            let df = o.windows(2).map(|w| (w[1] * w[0].conj()).arg()).sum::<f32>() / (o.len() - 1) as f32;
            assert!((df - want).abs() < 1e-4, "freq {df} vs {want}");
            let a = o.iter().map(|v| v.norm()).sum::<f32>() / o.len() as f32;
            assert!((a - amp).abs() < 0.01 * amp.max(0.1), "amplitude {a} vs {amp}");
        }
    }
}
