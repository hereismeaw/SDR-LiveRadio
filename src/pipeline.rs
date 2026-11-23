//! From an IQ capture to per-station RDS groups.

use crate::dsp::{self, Dsp, MPX_LEN, MPX_RATE};
use crate::rds::{find_groups, BlockStats, Group};
use num_complex::Complex32 as C32;

pub struct StationResult {
    pub freq_hz: f64,
    pub level_db: f32,
    pub pilot_db: f32,
    pub rds_snr_db: f32,
    pub groups: Vec<Group>,
    pub stats: BlockStats,
}

impl StationResult {
    pub fn quality(&self) -> f32 {
        if self.stats.total == 0 {
            0.0
        } else {
            self.stats.ok as f32 / self.stats.total as f32
        }
    }
}

/// IQ rate must be an integer multiple (>= 2) of the 228 kHz MPX rate.
pub fn decimation(fs: f64) -> Result<usize, String> {
    let d = fs / MPX_RATE;
    let r = d.round();
    if (d - r).abs() > 1e-9 || r < 2.0 {
        return Err(format!("sample rate {fs} Hz must be a multiple of 228 kHz"));
    }
    Ok(r as usize)
}

/// Number of IQ samples per capture (one MPX block of 2^19 samples, ~2.3 s).
pub fn capture_len(fs: f64) -> Result<usize, String> {
    Ok(decimation(fs)? * MPX_LEN)
}

/// Fraction of samples near ADC full scale.
pub fn clip_fraction(iq: &[C32]) -> f32 {
    let n = iq.iter().filter(|c| c.re.abs() > 0.98 || c.im.abs() > 0.98).count();
    n as f32 / iq.len().max(1) as f32
}

/// Channel power in dBFS within +-half_bw around `offset` Hz.
pub fn level_db(spec: &[C32], fs: f64, offset: f64, half_bw: f64) -> f32 {
    let n = spec.len() as i64;
    let bin = fs / n as f64;
    let k0 = (offset / bin).round() as i64;
    let h = (half_bw / bin).round() as i64;
    let s: f64 = (-h..=h).map(|r| spec[(k0 + r).rem_euclid(n) as usize].norm_sqr() as f64).sum();
    let nf = n as f64;
    (10.0 * (s / (nf * nf) + 1e-30).log10()) as f32
}

/// Find FM carriers on a 50 kHz grid inside [lo, hi). Returns (absolute Hz, dBFS).
pub fn detect_stations(spec: &[C32], fs: f64, center: f64, lo: f64, hi: f64) -> Vec<(f64, f32)> {
    let n = spec.len();
    let nf = n as f64;
    let bin = fs / nf;
    const CELL: f64 = 5_000.0;
    const STEP: f64 = 50_000.0;
    let ncell = (fs / CELL).ceil() as usize;
    let mut cells = vec![0f64; ncell];
    for (k, v) in spec.iter().enumerate() {
        let rel = if k < n / 2 { k as f64 } else { k as f64 - nf };
        let c = (((rel * bin + fs / 2.0) / CELL).floor().max(0.0) as usize).min(ncell - 1);
        cells[c] += v.norm_sqr() as f64;
    }
    // power within +-80 kHz of an offset
    let power = |off: f64| -> Option<f64> {
        let c0 = ((off + fs / 2.0) / CELL).floor() as i64;
        if c0 < 16 || c0 + 16 >= ncell as i64 {
            return None;
        }
        Some((c0 - 16..=c0 + 16).map(|c| cells[c as usize]).sum())
    };
    let edge = fs / 2.0 - 120e3;
    let mut grid: Vec<(f64, f64)> = Vec::new();
    let mut f = ((lo - 200e3) / STEP).ceil() * STEP;
    while f < hi + 200e3 {
        let off = f - center;
        if off.abs() <= edge {
            if let Some(p) = power(off) {
                grid.push((f, p));
            }
        }
        f += STEP;
    }
    if grid.len() < 5 {
        return vec![];
    }
    let mut sorted: Vec<f64> = grid.iter().map(|g| g.1).collect();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let floor = sorted[sorted.len() / 10].max(1e-30);

    let mut peaks = Vec::new();
    for i in 0..grid.len() {
        let (f, p) = grid[i];
        if f < lo || f >= hi || !(87.5e6 - 1.0..=108.0e6 + 1.0).contains(&f) || p < floor * 10.0 {
            continue;
        }
        let l = if i > 0 { grid[i - 1].1 } else { 0.0 };
        let r = if i + 1 < grid.len() { grid[i + 1].1 } else { 0.0 };
        if p >= l && p >= r {
            peaks.push((f, p));
        }
    }
    peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut accepted: Vec<(f64, f64)> = Vec::new();
    for (f, p) in peaks {
        let shadowed = accepted.iter().any(|&(af, ap)| {
            let df = (af - f).abs();
            df < 150e3 || (df < 400e3 && p * 10f64.powf(2.5) < ap)
        });
        if !shadowed {
            accepted.push((f, p));
        }
    }
    // An FM spectrum is symmetric about the carrier: the power centroid within
    // +-100 kHz finds the carrier better than the 50 kHz grid maximum (which can pick a
    // neighbour of nearly equal power). Snap the centroid back to the 50 kHz grid.
    let mut refined: Vec<(f64, f64)> = accepted
        .into_iter()
        .map(|(f, p)| {
            let c0 = ((f - center + fs / 2.0) / CELL).floor() as i64;
            let (mut sw, mut swf) = (0f64, 0f64);
            for c in (c0 - 20)..=(c0 + 20) {
                if c >= 0 && (c as usize) < ncell {
                    let w = cells[c as usize];
                    sw += w;
                    swf += w * ((c as f64 + 0.5) * CELL - fs / 2.0 + center);
                }
            }
            let fc = if sw > 0.0 { ((swf / sw) / STEP).round() * STEP } else { f };
            (if (fc - f).abs() <= STEP { fc } else { f }, p)
        })
        .collect();
    refined.sort_by(|a, b| a.0.total_cmp(&b.0));
    refined.dedup_by(|a, b| (a.0 - b.0).abs() < 1.0);
    refined
        .into_iter()
        .map(|(f, p)| (f, (10.0 * (p / (nf * nf)).log10()) as f32))
        .collect()
}

/// Power spectrum on an absolute `step` Hz grid inside [lo, hi] (dBFS per cell),
/// excluding the anti-alias filter edges of the capture.
pub fn band_spectrum(spec: &[C32], fs: f64, center: f64, lo: f64, hi: f64, step: f64) -> Vec<(f64, f32)> {
    let n = spec.len();
    let nf = n as f64;
    let bin = fs / nf;
    let lo = lo.max(center - fs / 2.0 + 100e3);
    let hi = hi.min(center + fs / 2.0 - 100e3);
    let first = (lo / step).ceil();
    let last = (hi / step).floor();
    if last < first {
        return vec![];
    }
    let cells = (last - first) as usize + 1;
    let mut acc = vec![0f64; cells];
    for (k, v) in spec.iter().enumerate() {
        let rel = if k < n / 2 { k as f64 } else { k as f64 - nf };
        let idx = ((center + rel * bin) / step).round() - first;
        if idx >= 0.0 && idx < cells as f64 {
            acc[idx as usize] += v.norm_sqr() as f64;
        }
    }
    acc.iter()
        .enumerate()
        .map(|(i, &p)| ((first + i as f64) * step, (10.0 * (p / (nf * nf) + 1e-30).log10()) as f32))
        .collect()
}

pub const SPECTRUM_STEP: f64 = 25_000.0;

/// Demodulate one station from the full-capture spectrum and decode its RDS groups.
pub fn decode_station(dsp: &mut Dsp, spec: &[C32], fs: f64, center: f64, freq_hz: f64) -> StationResult {
    let bin = fs / spec.len() as f64;
    let k0 = ((freq_hz - center) / bin).round() as i64;
    let y = dsp.extract(spec, k0, MPX_LEN, bin, 100e3, 114e3);
    let mpx = dsp::fm_demod(&y);
    let info = dsp.analyze_mpx(&mpx);
    let bits = dsp::rds_bits(&info.rds_bb, info.pilot_bb.as_deref());
    let (groups, stats) = find_groups(&bits);
    StationResult {
        freq_hz,
        level_db: level_db(spec, fs, freq_hz - center, 80e3),
        pilot_db: info.pilot_db,
        rds_snr_db: info.rds_snr_db,
        groups,
        stats,
    }
}

/// Everything one capture produced.
pub struct CaptureOutput {
    pub stations: Vec<StationResult>,
    pub clip: f32,
    /// (absolute Hz, dBFS) on the SPECTRUM_STEP grid.
    pub spectrum: Vec<(f64, f32)>,
}

/// Wideband capture: detect all stations in [lo, hi) and decode each.
pub fn process_capture(
    dsp: &mut Dsp,
    mut iq: Vec<C32>,
    fs: f64,
    center: f64,
    lo: f64,
    hi: f64,
) -> Result<CaptureOutput, String> {
    if iq.len() != capture_len(fs)? {
        return Err(format!("capture length {} != {}", iq.len(), capture_len(fs)?));
    }
    let clip = clip_fraction(&iq);
    dsp.fft(&mut iq);
    let found = detect_stations(&iq, fs, center, lo, hi);
    let stations = found
        .into_iter()
        .map(|(f, _)| decode_station(dsp, &iq, fs, center, f))
        .collect();
    let spectrum = band_spectrum(&iq, fs, center, lo, hi, SPECTRUM_STEP);
    Ok(CaptureOutput { stations, clip, spectrum })
}

/// Single known station (tune mode).
pub fn process_single(
    dsp: &mut Dsp,
    mut iq: Vec<C32>,
    fs: f64,
    center: f64,
    freq_hz: f64,
) -> Result<CaptureOutput, String> {
    if iq.len() != capture_len(fs)? {
        return Err(format!("capture length {} != {}", iq.len(), capture_len(fs)?));
    }
    let clip = clip_fraction(&iq);
    dsp.fft(&mut iq);
    let station = decode_station(dsp, &iq, fs, center, freq_hz);
    let spectrum = band_spectrum(&iq, fs, center, center - fs / 2.0, center + fs / 2.0, SPECTRUM_STEP);
    Ok(CaptureOutput { stations: vec![station], clip, spectrum })
}
