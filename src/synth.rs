//! Synthetic FM + RDS signal generator and an end-to-end self test (no hardware needed).

use crate::dsp::{Dsp, MPX_LEN, MPX_RATE, RDS_BITRATE};
use crate::pipeline;
use crate::rds::{encode_block, OFF_A, OFF_B, OFF_C, OFF_D};
use crate::station::Station;
use crate::text;
use num_complex::Complex32 as C32;
use std::f64::consts::TAU;

pub struct SynthStation {
    pub freq_hz: f64,
    pub amp: f32,
    pub pi: u16,
    pub ps: [u8; 8],
    pub rt: Vec<u8>,
    pub pty: u8,
    pub stereo: bool,
    /// Transmitter frequency error, simulates mistuning.
    pub freq_err_hz: f64,
    /// (MJD, UTC hour, minute, local offset in half hours)
    pub ct: Option<(u32, u16, u16, i8)>,
}

pub struct Rng(pub u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        (-2.0 * self.uniform().ln()).sqrt() * (TAU * self.uniform()).cos()
    }
}

/// One repetition cycle of groups: 0A x4 (PS), 2A (RT), optional 4A (CT).
pub fn groups(st: &SynthStation) -> Vec<[u16; 4]> {
    let pty = (st.pty as u16) << 5;
    let mut gs = Vec::new();
    for seg in 0..4u16 {
        let di = if seg == 3 && st.stereo { 1 << 2 } else { 0 };
        let b = pty | (1 << 3) | di | seg;
        let af = (225u16 << 8) | 80; // "1 AF follows", 95.5 MHz
        let d = ((st.ps[seg as usize * 2] as u16) << 8) | st.ps[seg as usize * 2 + 1] as u16;
        gs.push([st.pi, b, af, d]);
    }
    let mut rt = st.rt.clone();
    if rt.len() < 64 {
        rt.push(0x0D);
    }
    while rt.len() % 4 != 0 {
        rt.push(b' ');
    }
    for seg in 0..rt.len() / 4 {
        let b = (2 << 12) | pty | seg as u16;
        let r = &rt[seg * 4..seg * 4 + 4];
        gs.push([st.pi, b, ((r[0] as u16) << 8) | r[1] as u16, ((r[2] as u16) << 8) | r[3] as u16]);
    }
    if let Some((mjd, h, m, off)) = st.ct {
        let b = (4 << 12) | pty | ((mjd >> 15) & 3) as u16;
        let c = (((mjd & 0x7FFF) << 1) as u16) | (h >> 4);
        let d = ((h & 0xF) << 12) | (m << 6) | (if off < 0 { 1 << 5 } else { 0 }) | (off.unsigned_abs() as u16 & 0x1F);
        gs.push([st.pi, b, c, d]);
    }
    gs
}

pub fn bitstream(groups: &[[u16; 4]]) -> Vec<u8> {
    let mut bits = Vec::new();
    for g in groups {
        for (j, &data) in g.iter().enumerate() {
            let w = encode_block(data, [OFF_A, OFF_B, OFF_C, OFF_D][j]);
            bits.extend((0..26).rev().map(|k| ((w >> k) & 1) as u8));
        }
    }
    bits
}

/// FM multiplex at 228 kHz: 1 kHz tone, optional 19 kHz pilot, RDS at 57 kHz.
pub fn mpx(dsp: &mut Dsp, st: &SynthStation, len: usize, rng: &mut Rng) -> Vec<f32> {
    mpx_groups(dsp, &groups(st), st.stereo, len, rng)
}

fn mpx_groups(dsp: &mut Dsp, groups: &[[u16; 4]], stereo: bool, len: usize, rng: &mut Rng) -> Vec<f32> {
    let bits = bitstream(groups);
    let spbit = (MPX_RATE / RDS_BITRATE).round() as usize; // 192
    let half = spbit / 2;
    let mut base = vec![0f32; len];
    let mut e = 0u8;
    let skip = (rng.next_u64() % bits.len() as u64) as usize; // start mid-stream
    for (i, &b) in bits.iter().cycle().skip(skip).enumerate() {
        let s0 = i * spbit;
        if s0 >= len {
            break;
        }
        e ^= b; // differential encoding
        let s = if e == 1 { 1.0 } else { -1.0 };
        for k in 0..spbit.min(len - s0) {
            base[s0 + k] = if k < half { s } else { -s };
        }
    }
    let base = dsp.lowpass_real(&base, MPX_RATE, 2_400.0, 3_000.0);
    let peak = base.iter().fold(0f32, |m, v| m.max(v.abs())).max(1e-9);
    let phi0 = rng.uniform() * TAU;
    (0..len)
        .map(|n| {
            let t = n as f64 / MPX_RATE;
            let audio = 0.7 * (TAU * 1000.0 * t).cos();
            let pilot = if stereo { 0.09 * (TAU * 19_000.0 * t).sin() } else { 0.0 };
            let rds = 0.06 * (base[n] / peak) as f64 * (TAU * 57_000.0 * t + phi0).cos();
            (audio + pilot + rds) as f32
        })
        .collect()
}

/// FM-modulate `mpx` (228 kHz) onto a carrier at `offset_hz` and add into `iq` (rate 228 kHz * d).
pub fn fm_add(dsp: &mut Dsp, mpx: &[f32], d: usize, offset_hz: f64, amp: f32, iq: &mut [C32]) {
    let l = mpx.len();
    let n = l * d;
    let fs = MPX_RATE * d as f64;
    let mut s: Vec<C32> = mpx.iter().map(|&v| C32::new(v, 0.0)).collect();
    dsp.fft(&mut s);
    let mut up = vec![C32::default(); n];
    up[..l / 2].copy_from_slice(&s[..l / 2]);
    up[n - l / 2..].copy_from_slice(&s[l / 2..]);
    dsp.ifft(&mut up);
    let scale = 1.0 / l as f64;
    let mut phase = 0f64;
    for (k, out) in iq.iter_mut().enumerate().take(n) {
        let m = up[k].re as f64 * scale;
        phase = (phase + TAU * (75_000.0 * m + offset_hz) / fs) % TAU;
        *out += C32::from_polar(amp, phase as f32);
    }
}

/// Fraction of RDS blocks passing CRC on a noisy MPX, decoded (coherent-with-pilot, squaring).
pub fn rds_block_rate(noise_rms: f32, seed: u64) -> (f32, f32) {
    use crate::rds::find_groups;
    let mut dsp = Dsp::new();
    let mut rng = Rng(seed);
    let st = SynthStation {
        freq_hz: 0.0,
        amp: 1.0,
        pi: 0x2A01,
        ps: *b"WEAK RDS",
        rt: b"sensitivity test".to_vec(),
        pty: 1,
        stereo: true,
        freq_err_hz: 0.0,
        ct: None,
    };
    let mut m = mpx(&mut dsp, &st, MPX_LEN, &mut rng);
    for v in m.iter_mut() {
        *v += noise_rms * rng.gauss() as f32;
    }
    let info = dsp.analyze_mpx(&m);
    let rate = |bits: Vec<u8>| {
        let (_, s) = find_groups(&bits);
        if s.total == 0 { 0.0 } else { s.ok as f32 / s.total as f32 }
    };
    let pilot = dsp.extract_pilot(&m);
    (rate(crate::dsp::rds_bits(&info.rds_bb, Some(&pilot))), rate(crate::dsp::rds_bits(&info.rds_bb, None)))
}

/// Two synthetic stations (one strong stereo with Thai RT, one weak mono) through the real pipeline.
pub fn selftest() -> Result<String, String> {
    let mut dsp = Dsp::new();
    let d = 20usize;
    let fs = MPX_RATE * d as f64;
    let n = d * MPX_LEN;
    let center = 97.125e6;
    let (thai, _, _) = encoding_rs::WINDOWS_874.encode("ทดสอบ RDS ภาษาไทย");
    let stations = [
        SynthStation {
            freq_hz: 96.5e6,
            amp: 0.25,
            pi: 0x2A01,
            ps: *b"BKK RDS1",
            rt: thai.into_owned(),
            pty: 10,
            stereo: true,
            freq_err_hz: 1_200.0,
            ct: Some((61367, 8, 30, 14)),
        },
        SynthStation {
            freq_hz: 98.0e6,
            amp: 0.03,
            pi: 0x2B02,
            ps: *b"SECOND  ",
            rt: b"Hello Bangkok from a synthetic RDS encoder".to_vec(),
            pty: 3,
            stereo: false,
            freq_err_hz: -800.0,
            ct: None,
        },
    ];
    let mut rng = Rng(0x1234_5678_9ABC_DEF0);
    let mut iq: Vec<C32> =
        (0..n).map(|_| C32::new((rng.gauss() * 0.003) as f32, (rng.gauss() * 0.003) as f32)).collect();
    for (i, s) in stations.iter().enumerate() {
        let mut cycle = groups(s);
        if i == 0 {
            let pty = u16::from(s.pty) << 5;
            let event = [s.pi, 0x800B | pty, 101, 12345];
            cycle.extend([[s.pi, 0x3010 | pty, 17 << 6, 0xCD46], event, event]);
        }
        let m = mpx_groups(&mut dsp, &cycle, s.stereo, MPX_LEN, &mut rng);
        fm_add(&mut dsp, &m, d, s.freq_hz - center + s.freq_err_hz, s.amp, &mut iq);
    }

    let t0 = std::time::Instant::now();
    let out = pipeline::process_capture(&mut dsp, iq, fs, center, center - 2e6, center + 2e6)?;
    let results = out.stations;
    let mut report = format!(
        "Processed {} samples ({:.2} s of signal) in {:.2} s\nStations found: {}\n",
        n,
        n as f64 / fs,
        t0.elapsed().as_secs_f64(),
        results.iter().map(|r| format!("{:.2}", r.freq_hz / 1e6)).collect::<Vec<_>>().join(", ")
    );
    if results.len() != stations.len() {
        return Err(format!("{report}FAIL: found {} stations, expected {}", results.len(), stations.len()));
    }
    for (i, s) in stations.iter().enumerate() {
        let r = results
            .iter()
            .find(|r| (r.freq_hz - s.freq_hz).abs() < 1.0)
            .ok_or_else(|| format!("{report}station {:.2} MHz not found", s.freq_hz / 1e6))?;
        let mut st = Station::new((s.freq_hz / 1e3) as u32);
        for g in &r.groups {
            st.apply_group(g);
        }
        report += &format!(
            "  {:.2} MHz  level {:.1} dBFS  pilot {:.1} dB  RDS {:.1} dB  blocks {}/{} (corrected {})  groups {}\n",
            s.freq_hz / 1e6,
            r.level_db,
            r.pilot_db,
            r.rds_snr_db,
            r.stats.ok,
            r.stats.total,
            r.stats.corrected,
            r.groups.len()
        );
        let fail = |what: &str| Err(format!("{report}FAIL at {:.2} MHz: {what}", s.freq_hz / 1e6));
        if st.pi != Some(s.pi) {
            return fail(&format!("PI {:?}", st.pi));
        }
        if st.ps.as_deref() != Some(&s.ps[..]) {
            return fail(&format!("PS {:?}", st.ps.as_ref().map(|p| text::decode(p).0)));
        }
        if st.rt.as_deref() != Some(&s.rt[..]) {
            return fail(&format!("RT {:?}", st.rt.as_ref().map(|p| text::decode(p).0)));
        }
        if s.ct.is_some() && st.ct.as_deref() != Some("2026-11-23 15:30 (UTC+07:00)") {
            return fail(&format!("CT {:?}", st.ct));
        }
        if (s.pty, s.stereo) != (st.pty.unwrap_or(99), r.pilot_db > 15.0) {
            return fail("PTY / pilot");
        }
        if i == 0 && !st.tmc.messages.iter().any(|m| (m.ltn, m.event, m.location) == (17, 101, 12345)) {
            return fail("ALERT-C event missing");
        }
        report += &format!(
            "    PI {:04X}  PS \"{}\"  RT \"{}\" [{}]{}\n",
            s.pi,
            text::decode(st.ps.as_ref().unwrap()).0,
            text::decode(st.rt.as_ref().unwrap()).0,
            text::decode(st.rt.as_ref().unwrap()).1,
            st.ct.as_ref().map(|c| format!("  CT {c}")).unwrap_or_default()
        );
        if let Some(m) = st.tmc.messages.first() {
            report += &format!("    TMC event {} at location {} (LTN {})\n", m.event, m.location, m.ltn);
        }
    }
    report += "Self test: PASS";
    Ok(report)
}
