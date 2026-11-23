//! Audio recording: WAV directly; FLAC / MP3 (LAME) / AAC (FDK, ADTS) by recording a
//! float WAV first and transcoding it when the recording stops (bounded memory,
//! nothing lost if the encoder is slow).

use crate::audio::{Frame, Resampler};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Wav,
    Flac,
    Mp3,
    Aac,
}

impl Format {
    pub const ALL: [Format; 4] = [Format::Wav, Format::Flac, Format::Mp3, Format::Aac];
    pub fn name(self) -> &'static str {
        match self {
            Format::Wav => "WAV",
            Format::Flac => "FLAC",
            Format::Mp3 => "MP3",
            Format::Aac => "AAC",
        }
    }
    pub fn ext(self) -> &'static str {
        match self {
            Format::Wav => "wav",
            Format::Flac => "flac",
            Format::Mp3 => "mp3",
            Format::Aac => "aac",
        }
    }
    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.name().eq_ignore_ascii_case(s))
    }
}

#[derive(Clone, Debug)]
pub struct RecordSettings {
    pub format: Format,
    /// WAV: 16, 24 or 32 (float).
    pub wav_bits: u16,
    /// FLAC: 16 or 24.
    pub flac_bits: u16,
    pub mp3_kbps: u32,
    pub aac_kbps: u32,
    /// HE-AAC (SBR) instead of AAC-LC; good below ~64 kbps.
    pub aac_he: bool,
    pub sample_rate: u32,
    pub stereo: bool,
    pub folder: PathBuf,
}

impl Default for RecordSettings {
    fn default() -> Self {
        let folder = std::env::var_os("USERPROFILE")
            .map(|h| PathBuf::from(h).join("Music").join("RDS FM Recordings"))
            .unwrap_or_else(|| PathBuf::from("recordings"));
        Self {
            format: Format::Mp3,
            wav_bits: 16,
            flac_bits: 16,
            mp3_kbps: 192,
            aac_kbps: 128,
            aac_he: false,
            sample_rate: 48_000,
            stereo: true,
            folder,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub comment: String,
}

enum Msg {
    Audio(Vec<Frame>),
    Stop,
}

pub type Log = Arc<dyn Fn(String) + Send + Sync>;

pub struct Recorder {
    tx: mpsc::Sender<Msg>,
    pub path: PathBuf,
    pub started: Instant,
    frames: Arc<AtomicU64>,
    rate: u32,
}

fn unique_path(folder: &Path, base: &str, ext: &str) -> PathBuf {
    let clean: String = base.chars().map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c }).collect();
    let mut p = folder.join(format!("{clean}.{ext}"));
    let mut i = 1;
    while p.exists() {
        p = folder.join(format!("{clean}-{i}.{ext}"));
        i += 1;
    }
    p
}

impl Recorder {
    /// Start recording audio arriving at `in_rate` Hz.
    pub fn start(s: &RecordSettings, in_rate: f64, base_name: &str, tags: Tags, log: Log) -> Result<Self, String> {
        std::fs::create_dir_all(&s.folder).map_err(|e| format!("cannot create {}: {e}", s.folder.display()))?;
        let path = unique_path(&s.folder, base_name, s.format.ext());
        let wav_path = if s.format == Format::Wav { path.clone() } else { unique_path(&s.folder, base_name, "part.wav") };
        let channels = if s.stereo { 2 } else { 1 };
        let (bits, float) = match (s.format, s.wav_bits) {
            (Format::Wav, 16) => (16, false),
            (Format::Wav, 24) => (24, false),
            (Format::Wav, _) | (_, _) => (32, true),
        };
        let spec = hound::WavSpec {
            channels,
            sample_rate: s.sample_rate,
            bits_per_sample: bits,
            sample_format: if float { hound::SampleFormat::Float } else { hound::SampleFormat::Int },
        };
        let mut writer = hound::WavWriter::create(&wav_path, spec).map_err(|e| format!("cannot create {}: {e}", wav_path.display()))?;
        let (tx, rx) = mpsc::channel::<Msg>();
        let frames = Arc::new(AtomicU64::new(0));
        let fcount = frames.clone();
        let settings = s.clone();
        let final_path = path.clone();
        std::thread::spawn(move || {
            let mut rs = Resampler::new(in_rate, settings.sample_rate as f64);
            let mut buf = Vec::new();
            let mut err: Option<String> = None;
            while let Ok(Msg::Audio(block)) = rx.recv() {
                buf.clear();
                rs.process(&block, &mut buf);
                for f in &buf {
                    let ch: &[f32] = if channels == 2 { f } else { &[0.5 * (f[0] + f[1])] };
                    for &v in ch {
                        let v = v.clamp(-1.0, 1.0);
                        let r = match (float, bits) {
                            (true, _) => writer.write_sample(v),
                            (false, 16) => writer.write_sample((v * 32767.0).round() as i16),
                            _ => writer.write_sample((v * 8_388_607.0).round() as i32),
                        };
                        if let Err(e) = r {
                            err.get_or_insert(e.to_string());
                        }
                    }
                }
                fcount.fetch_add(buf.len() as u64, Ordering::Relaxed);
            }
            if let Err(e) = writer.finalize() {
                err.get_or_insert(e.to_string());
            }
            if let Some(e) = err {
                log(format!("Recording error: {e}"));
                return;
            }
            if settings.format == Format::Wav {
                log(format!("Recording saved: {}", final_path.display()));
                return;
            }
            log(format!("Encoding {} …", settings.format.name()));
            let t0 = Instant::now();
            let res = match settings.format {
                Format::Flac => transcode_flac(&wav_path, &final_path, settings.flac_bits),
                Format::Mp3 => transcode_mp3(&wav_path, &final_path, settings.mp3_kbps, &tags),
                Format::Aac => transcode_aac(&wav_path, &final_path, settings.aac_kbps, settings.aac_he),
                Format::Wav => unreachable!(),
            };
            match res {
                Ok(()) => {
                    let _ = std::fs::remove_file(&wav_path);
                    let size = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
                    log(format!(
                        "Recording saved: {} ({:.1} MB, encoded in {:.1} s)",
                        final_path.display(),
                        size as f64 / 1e6,
                        t0.elapsed().as_secs_f64()
                    ));
                }
                Err(e) => log(format!("Encoding failed ({e}); the WAV is kept: {}", wav_path.display())),
            }
        });
        Ok(Self { tx, path, started: Instant::now(), frames, rate: s.sample_rate })
    }

    pub fn push(&self, frames: &[Frame]) {
        let _ = self.tx.send(Msg::Audio(frames.to_vec()));
    }

    pub fn seconds(&self) -> f64 {
        self.frames.load(Ordering::Relaxed) as f64 / self.rate as f64
    }

    /// Finish; encoding (if any) continues in the background and reports via the log.
    pub fn stop(self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

/// Read a float/int WAV as f32 interleaved chunks.
struct WavIn {
    r: hound::WavReader<BufReader<File>>,
    pub channels: usize,
    pub rate: u32,
}

impl WavIn {
    fn open(p: &Path) -> Result<Self, String> {
        let r = hound::WavReader::open(p).map_err(|e| e.to_string())?;
        let s = r.spec();
        Ok(Self { channels: s.channels as usize, rate: s.sample_rate, r })
    }
    /// Up to `frames` frames, interleaved; empty at end.
    fn read(&mut self, frames: usize) -> Vec<f32> {
        let s = self.r.spec();
        let n = frames * self.channels;
        match (s.sample_format, s.bits_per_sample) {
            (hound::SampleFormat::Float, _) => self.r.samples::<f32>().take(n).filter_map(Result::ok).collect(),
            (_, 16) => self.r.samples::<i16>().take(n).filter_map(Result::ok).map(|v| v as f32 / 32768.0).collect(),
            _ => self.r.samples::<i32>().take(n).filter_map(Result::ok).map(|v| v as f32 / 8_388_608.0).collect(),
        }
    }
}

fn to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

fn transcode_flac(src: &Path, dst: &Path, bits: u16) -> Result<(), String> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;
    struct Src {
        w: WavIn,
        bits: usize,
    }
    impl flacenc::source::Source for Src {
        fn channels(&self) -> usize {
            self.w.channels
        }
        fn bits_per_sample(&self) -> usize {
            self.bits
        }
        fn sample_rate(&self) -> usize {
            self.w.rate as usize
        }
        fn read_samples<F: flacenc::source::Fill>(
            &mut self,
            block_size: usize,
            dest: &mut F,
        ) -> Result<usize, flacenc::error::SourceError> {
            let scale = ((1i64 << (self.bits - 1)) - 1) as f32;
            let x: Vec<i32> = self.w.read(block_size).iter().map(|v| (v.clamp(-1.0, 1.0) * scale).round() as i32).collect();
            dest.fill_interleaved(&x)?;
            Ok(x.len() / self.w.channels)
        }
    }
    let bits = if bits == 24 { 24 } else { 16 };
    let src = Src { w: WavIn::open(src)?, bits };
    let config = flacenc::config::Encoder::default().into_verified().map_err(|(_, e)| format!("{e:?}"))?;
    let stream = flacenc::encode_with_fixed_block_size(&config, src, 4096).map_err(|e| format!("{e:?}"))?;
    let mut sink = flacenc::bitsink::MemSink::<u8>::new();
    stream.write(&mut sink).map_err(|e| format!("{e:?}"))?;
    std::fs::write(dst, sink.as_slice()).map_err(|e| e.to_string())
}

fn transcode_mp3(src: &Path, dst: &Path, kbps: u32, tags: &Tags) -> Result<(), String> {
    use mp3lame_encoder::{Bitrate, Builder, DualPcm, FlushNoGap, Id3Tag, MonoPcm, Quality};
    let mut w = WavIn::open(src)?;
    let br = match kbps {
        0..=96 => Bitrate::Kbps96,
        97..=128 => Bitrate::Kbps128,
        129..=160 => Bitrate::Kbps160,
        161..=192 => Bitrate::Kbps192,
        193..=224 => Bitrate::Kbps224,
        225..=256 => Bitrate::Kbps256,
        _ => Bitrate::Kbps320,
    };
    let mut b = Builder::new().ok_or("LAME init failed")?;
    b.set_num_channels(w.channels as u8).map_err(|e| format!("{e:?}"))?;
    b.set_sample_rate(w.rate).map_err(|e| format!("{e:?}"))?;
    b.set_brate(br).map_err(|e| format!("{e:?}"))?;
    b.set_quality(Quality::Best).map_err(|e| format!("{e:?}"))?;
    let _ = b.set_id3_tag(Id3Tag {
        title: tags.title.as_bytes(),
        artist: tags.artist.as_bytes(),
        album: b"RDS FM recording",
        album_art: &[],
        year: b"",
        comment: tags.comment.as_bytes(),
    });
    let mut enc = b.build().map_err(|e| format!("{e:?}"))?;
    let mut out = BufWriter::new(File::create(dst).map_err(|e| e.to_string())?);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let x = w.read(4608);
        if x.is_empty() {
            break;
        }
        buf.clear();
        buf.reserve(x.len() * 5 / 4 + 7200);
        if w.channels == 2 {
            let l: Vec<i16> = x.iter().step_by(2).map(|&v| to_i16(v)).collect();
            let r: Vec<i16> = x.iter().skip(1).step_by(2).map(|&v| to_i16(v)).collect();
            enc.encode_to_vec(DualPcm { left: &l, right: &r }, &mut buf).map_err(|e| format!("{e:?}"))?;
        } else {
            let m: Vec<i16> = x.iter().map(|&v| to_i16(v)).collect();
            enc.encode_to_vec(MonoPcm(&m), &mut buf).map_err(|e| format!("{e:?}"))?;
        }
        out.write_all(&buf).map_err(|e| e.to_string())?;
    }
    buf.clear();
    buf.reserve(7200);
    enc.flush_to_vec::<FlushNoGap>(&mut buf).map_err(|e| format!("{e:?}"))?;
    out.write_all(&buf).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}

fn transcode_aac(src: &Path, dst: &Path, kbps: u32, he: bool) -> Result<(), String> {
    use fdk_aac::enc::{AudioObjectType, BitRate, ChannelMode, Encoder, EncoderParams, Transport};
    let mut w = WavIn::open(src)?;
    let enc = Encoder::new(EncoderParams {
        bit_rate: BitRate::Cbr(kbps * 1000),
        sample_rate: w.rate,
        transport: Transport::Adts,
        channels: if w.channels == 2 { ChannelMode::Stereo } else { ChannelMode::Mono },
        audio_object_type: if he { AudioObjectType::Mpeg4HeAac } else { AudioObjectType::Mpeg4LowComplexity },
    })
    .map_err(|e| format!("FDK-AAC: {e:?}"))?;
    let mut out = BufWriter::new(File::create(dst).map_err(|e| e.to_string())?);
    let mut obuf = vec![0u8; 16 * 1024];
    loop {
        let x = w.read(1024);
        if x.is_empty() {
            break;
        }
        let pcm: Vec<i16> = x.iter().map(|&v| to_i16(v)).collect();
        let mut off = 0;
        while off < pcm.len() {
            let info = enc.encode(&pcm[off..], &mut obuf).map_err(|e| format!("FDK-AAC: {e:?}"))?;
            out.write_all(&obuf[..info.output_size]).map_err(|e| e.to_string())?;
            if info.input_consumed == 0 {
                break;
            }
            off += info.input_consumed;
        }
    }
    out.flush().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_formats_produce_valid_files() {
        let dir = std::env::temp_dir().join(format!("rds-fm-rec-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = mpsc::channel::<String>();
        let tx = std::sync::Mutex::new(tx);
        let log: Log = Arc::new(move |m| {
            let _ = tx.lock().unwrap().send(m);
        });
        let fs = 45_600.0;
        let audio: Vec<Frame> = (0..(fs as usize * 2))
            .map(|i| {
                let t = i as f64 / fs;
                [(0.3 * (std::f64::consts::TAU * 440.0 * t).sin()) as f32, (0.3 * (std::f64::consts::TAU * 660.0 * t).sin()) as f32]
            })
            .collect();
        for format in Format::ALL {
            let s = RecordSettings { format, folder: dir.clone(), ..RecordSettings::default() };
            let tags = Tags { title: "Test".into(), artist: "RDS".into(), comment: "97.0 MHz".into() };
            let rec = Recorder::start(&s, fs, "test", tags, log.clone()).unwrap();
            for c in audio.chunks(4096) {
                rec.push(c);
            }
            let path = rec.path.clone();
            rec.stop();
            // wait for "saved" (or failure) for this file
            loop {
                let m = rx.recv_timeout(std::time::Duration::from_secs(30)).expect("no log");
                assert!(!m.contains("failed") && !m.contains("error"), "{m}");
                if m.starts_with("Recording saved") {
                    break;
                }
            }
            let data = std::fs::read(&path).unwrap();
            let magic_ok = match format {
                Format::Wav => data.starts_with(b"RIFF"),
                Format::Flac => data.starts_with(b"fLaC"),
                Format::Mp3 => data.starts_with(b"ID3") || (data[0] == 0xFF && data[1] & 0xE0 == 0xE0),
                Format::Aac => data[0] == 0xFF && data[1] & 0xF0 == 0xF0,
            };
            assert!(magic_ok, "{format:?}: bad header {:02X?}", &data[..4]);
            assert!(data.len() > 10_000, "{format:?}: only {} bytes", data.len());
            assert!(!dir.join("test.part.wav").exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
