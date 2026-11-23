//! Signal processing: FFT channel extraction, FM demodulation, RDS baseband and bit recovery.
//!
//! Rates are chosen so every step is an exact FFT slice:
//!   IQ capture  fs = 228 kHz * D, length D * 2^19
//!   FM MPX      228 kHz, 2^19 samples (~2.3 s); 57 kHz = fs/4, 19 kHz = fs/12
//!   RDS bb      28.5 kHz, 2^16 samples = 24 samples per RDS bit (1187.5 bit/s)

use num_complex::Complex32 as C32;
use rustfft::FftPlanner;

pub const MPX_RATE: f64 = 228_000.0;
pub const MPX_LEN: usize = 1 << 19;
pub const RDS_LEN: usize = 1 << 16;
pub const RDS_RATE: f64 = MPX_RATE * RDS_LEN as f64 / MPX_LEN as f64; // 28 500 Hz
pub const RDS_BITRATE: f64 = 1187.5;
pub const SPB: usize = 24; // samples per bit at RDS_RATE

pub struct Dsp {
    planner: FftPlanner<f32>,
}

impl Default for Dsp {
    fn default() -> Self {
        Self::new()
    }
}

/// Raised-cosine transition: 1 below `pass`, 0 above `stop`.
pub fn taper(f_abs: f64, pass: f64, stop: f64) -> f32 {
    if f_abs <= pass {
        1.0
    } else if f_abs >= stop {
        0.0
    } else {
        let x = (f_abs - pass) / (stop - pass);
        (0.5 * (1.0 + (std::f64::consts::PI * x).cos())) as f32
    }
}

/// Quadrature FM discriminator (output in radians per sample).
pub fn fm_demod(y: &[C32]) -> Vec<f32> {
    let mut out = vec![0f32; y.len()];
    for n in 1..y.len() {
        out[n] = (y[n] * y[n - 1].conj()).arg();
    }
    out
}

/// Sum of |X|^2 over positive-frequency bins [lo, hi] Hz; returns (power, bin count).
fn band_power(spec: &[C32], bin_hz: f64, lo: f64, hi: f64) -> (f64, usize) {
    let a = (lo / bin_hz).round() as usize;
    let b = ((hi / bin_hz).round() as usize).min(spec.len() - 1);
    let s: f64 = spec[a..=b].iter().map(|c| c.norm_sqr() as f64).sum();
    (s, b - a + 1)
}

pub struct MpxInfo {
    /// 19 kHz pilot vs. noise in the same bandwidth (dB). > ~15 dB means stereo.
    pub pilot_db: f32,
    /// (signal+noise)/noise density around 57 kHz (dB). ~0 dB means no RDS.
    pub rds_snr_db: f32,
    /// Complex RDS baseband at RDS_RATE.
    pub rds_bb: Vec<C32>,
    /// Complex 19 kHz pilot baseband (same rate/length) when the pilot is strong.
    pub pilot_bb: Option<Vec<C32>>,
}

impl Dsp {
    pub fn new() -> Self {
        Self { planner: FftPlanner::new() }
    }

    pub fn fft(&mut self, buf: &mut [C32]) {
        self.planner.plan_fft_forward(buf.len()).process(buf);
    }

    pub fn ifft(&mut self, buf: &mut [C32]) {
        self.planner.plan_fft_inverse(buf.len()).process(buf);
    }

    /// Take `m` bins around `center_bin` of `spec` (FFT order), apply a band-pass taper,
    /// and inverse-FFT: a frequency shift + filter + decimation in one step.
    pub fn extract(
        &mut self,
        spec: &[C32],
        center_bin: i64,
        m: usize,
        bin_hz: f64,
        pass_hz: f64,
        stop_hz: f64,
    ) -> Vec<C32> {
        let n = spec.len() as i64;
        let half = (m / 2) as i64;
        let mut y = vec![C32::default(); m];
        for (j, out) in y.iter_mut().enumerate() {
            let rel = if (j as i64) < half { j as i64 } else { j as i64 - m as i64 };
            let w = taper((rel as f64 * bin_hz).abs(), pass_hz, stop_hz);
            if w == 0.0 {
                continue;
            }
            let src = (center_bin + rel).rem_euclid(n) as usize;
            *out = spec[src] * w;
        }
        self.ifft(&mut y);
        y
    }

    /// Zero-phase low-pass of a real signal via FFT.
    pub fn lowpass_real(&mut self, x: &[f32], rate: f64, pass: f64, stop: f64) -> Vec<f32> {
        let n = x.len();
        let mut s: Vec<C32> = x.iter().map(|&v| C32::new(v, 0.0)).collect();
        self.fft(&mut s);
        let bin = rate / n as f64;
        for (k, v) in s.iter_mut().enumerate() {
            let rel = if k <= n / 2 { k as f64 } else { k as f64 - n as f64 };
            *v *= taper((rel * bin).abs(), pass, stop);
        }
        self.ifft(&mut s);
        let scale = 1.0 / n as f32;
        s.iter().map(|c| c.re * scale).collect()
    }

    /// Pilot baseband regardless of its SNR (for tests and diagnostics).
    pub fn extract_pilot(&mut self, mpx: &[f32]) -> Vec<C32> {
        let mut p: Vec<C32> = mpx.iter().map(|&v| C32::new(v, 0.0)).collect();
        self.fft(&mut p);
        self.pilot_from_spectrum(&p)
    }

    /// 19 kHz pilot at RDS_RATE, mixed down by exactly 19 kHz. 19 kHz is not on the FFT
    /// grid (57 kHz is), so the fractional bin is removed here; otherwise pilot^3 would
    /// drift against the 57 kHz baseband by one bin over the block.
    fn pilot_from_spectrum(&mut self, spec: &[C32]) -> Vec<C32> {
        let l = spec.len();
        let bin = MPX_RATE / l as f64;
        let exact = 19_000.0 / bin;
        let k19 = exact.round();
        let mut p = self.extract(spec, k19 as i64, RDS_LEN, bin, 15.0, 60.0);
        let m = p.len() as f64;
        let frac = exact - k19;
        for (i, v) in p.iter_mut().enumerate() {
            *v *= C32::from_polar(1.0, (-std::f64::consts::TAU * frac * i as f64 / m) as f32);
        }
        p
    }

    /// Measure pilot / RDS presence and extract the 57 kHz RDS baseband from an MPX signal.
    pub fn analyze_mpx(&mut self, mpx: &[f32]) -> MpxInfo {
        let l = mpx.len();
        let mut p: Vec<C32> = mpx.iter().map(|&v| C32::new(v, 0.0)).collect();
        self.fft(&mut p);
        let bin = MPX_RATE / l as f64;

        let (ps, pn) = band_power(&p, bin, 18_985.0, 19_015.0);
        let (ns, nn) = band_power(&p, bin, 17_000.0, 18_500.0);
        let noise_density = ns / nn as f64 + 1e-30;
        let pilot_db = (10.0 * (ps / (noise_density * pn as f64) + 1e-30).log10()) as f32;

        let (rs, rn) = band_power(&p, bin, 55_600.0, 58_400.0);
        let (qs, qn) = band_power(&p, bin, 60_500.0, 63_300.0);
        let rds_snr_db =
            (10.0 * ((rs / rn as f64) / (qs / qn as f64 + 1e-30) + 1e-30).log10()) as f32;

        let rds_bb = self.extract(&p, (57_000.0 / bin).round() as i64, RDS_LEN, bin, 2_200.0, 3_000.0);
        // Pilot at the same rate and length, so it is sample-aligned with rds_bb.
        let pilot_bb = (pilot_db > PILOT_COHERENT_DB).then(|| self.pilot_from_spectrum(&p));
        MpxInfo { pilot_db, rds_snr_db, rds_bb, pilot_bb }
    }
}

/// Above this pilot SNR the RDS carrier is derived from the pilot (coherent).
pub const PILOT_COHERENT_DB: f32 = 20.0;

/// Carrier-corrected real RDS baseband.
///
/// With a pilot, the 57 kHz carrier is the pilot's 3rd harmonic, so `pilot^3` is a
/// clean phase reference and only one constant phase (mod pi) remains to estimate.
/// Without one, BPSK squaring per 50 ms segment tracks the carrier.
fn derotate(z: &[C32], pilot: Option<&[C32]>) -> Vec<f32> {
    let n = z.len();
    if let Some(p) = pilot.filter(|p| p.len() == n) {
        let zc: Vec<C32> = z
            .iter()
            .zip(p)
            .map(|(&v, &q)| {
                let u = q / q.norm().max(1e-20);
                v * (u * u * u).conj()
            })
            .collect();
        let s2: C32 = zc.iter().map(|v| v * v).sum();
        let rot = C32::from_polar(1.0, -0.5 * s2.arg());
        return zc.iter().map(|v| (v * rot).re).collect();
    }
    let seg = (RDS_RATE * 0.05).round() as usize;
    let nseg = n / seg;
    if nseg < 2 {
        return vec![];
    }
    // 2*theta per segment, unwrapped
    let mut ph: Vec<f64> = (0..nseg)
        .map(|k| {
            let s: C32 = z[k * seg..(k + 1) * seg].iter().map(|v| v * v).sum();
            s.arg() as f64
        })
        .collect();
    let tau = std::f64::consts::TAU;
    for k in 1..nseg {
        let mut d = ph[k] - ph[k - 1];
        while d > std::f64::consts::PI {
            d -= tau;
        }
        while d < -std::f64::consts::PI {
            d += tau;
        }
        ph[k] = ph[k - 1] + d;
    }
    (0..n)
        .map(|i| {
            let pos = i as f64 / seg as f64 - 0.5;
            let two_theta = if pos <= 0.0 {
                ph[0]
            } else if pos >= (nseg - 1) as f64 {
                ph[nseg - 1]
            } else {
                let k = pos.floor() as usize;
                let fr = pos - k as f64;
                ph[k] * (1.0 - fr) + ph[k + 1] * fr
            };
            (z[i] * C32::from_polar(1.0, (-0.5 * two_theta) as f32)).re
        })
        .collect()
}

/// Recover differentially-decoded RDS data bits from the complex 57 kHz baseband.
///
/// 1. Carrier phase: from the pilot when present, else BPSK squaring (see `derotate`).
/// 2. Biphase matched filter (integrate first half-bit minus second half-bit).
/// 3. Symbol timing: the phase (of 24) with the largest mean |MF| output.
/// 4. Differential decoding removes the 180 degree ambiguity.
pub fn rds_bits(z: &[C32], pilot: Option<&[C32]>) -> Vec<u8> {
    let s = derotate(z, pilot);
    let n = s.len();
    if n < 2 * SPB {
        return vec![];
    }
    let mut cs = vec![0f64; n + 1];
    for i in 0..n {
        cs[i + 1] = cs[i] + s[i] as f64;
    }
    let h = SPB / 2;
    let mf = |i: usize| (cs[i + h] - cs[i]) - (cs[i + SPB] - cs[i + h]);

    let mut best = (0usize, -1f64);
    for p in 0..SPB {
        let mut acc = 0.0;
        let mut i = p;
        while i + SPB <= n {
            acc += mf(i).abs();
            i += SPB;
        }
        if acc > best.1 {
            best = (p, acc);
        }
    }
    let mut raw = Vec::with_capacity(n / SPB);
    let mut i = best.0;
    while i + SPB <= n {
        raw.push((mf(i) > 0.0) as u8);
        i += SPB;
    }
    raw.windows(2).map(|w| w[0] ^ w[1]).collect()
}
