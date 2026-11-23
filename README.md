<p align="center"><img src="assets/icon.png" width="96" alt=""></p>

# SDR-LiveRadio

**I built this for fun.** I wanted to see what Bangkok FM stations were sending under the music, so I plugged in a B210 clone and wrote an RDS decoder. Then I wanted to listen in stereo, keep recordings, and stream a few stations to another room. Now it's a Windows SDR radio written in Rust, with the DSP built from scratch.

> ⚠️ Hobby project. It works on my desk (LibreSDR B210mini on USB 2.0, Windows 11, Bangkok). Your mileage, antenna and local regulations may vary. **Receive only.** The app never transmits.

![screenshot](assets/screenshot.png)

## What it does

| | |
|---|---|
| 📡 **FM band scan** | Sweeps 87.5–108 MHz and decodes the RDS of every station it finds at the same time |
| 🎧 **Live radio** | FM stereo (pilot PLL, 50/75 µs de-emphasis), MW / SW / airband AM, with a real-time spectrum |
| 📝 **RDS, the whole thing** | PI (country/area), PS, RadioText, **RT+** (title/artist), PTY/PTYN, clock, AF, ECC, PIN, DI, Long PS, **EON**, ODA, eRT, BLER stats and raw groups |
| 🚦 **Traffic** | TP/TA and EON, plus **RDS-TMC ALERT-C** reception, Bangkok station shortcuts and location name import |
| 🎚️ **Audio** | 10-band EQ with presets, output device selection, auto stereo / forced mono |
| ✨ **"Make it sound like a CD"** | **AI noise reduction** (RNNoise neural net), bandwidth extension (harmonic exciter above the 15 kHz FM cut-off) and stereo width. Physics still applies: this adds plausible detail, it can't recover what was never broadcast. |
| ⏺️ **Recording** | WAV (16/24/32f), FLAC, MP3 (with ID3 tags taken from RDS), AAC-LC/HE, up to 96 kHz |
| 🌐 **Internet radio** | Built-in Icecast-style server (MP3 + ICY "now playing" from RDS) and push to a real Icecast server. **Several stations at once** from a single capture. |
| ⭐ **Presets & logbook** | Memory channels (CSV import/export) and a logbook of every station/PS/RT/TA/TMC event (CSV) |

## Hardware

| | LibreSDR B210mini / USRP B2xx | RTL-SDR Blog V4 |
|---|---|---|
| Driver | [UHD](https://files.ettus.com/binaries/uhd/) 4.x, C API loaded at runtime | `rtlsdr.dll` (RTL-SDR Blog build) + WinUSB via Zadig |
| Range | 50 MHz – 6 GHz (no MW/SW) | 500 kHz – 1.766 GHz (V4 HF upconverter) |
| FM scan | 4.56 MS/s, 4 MHz per hop | 2.28 MS/s, 2 MHz per hop |
| Multi-station streaming | ≈ ±1.8 MHz around the tuned station | ≈ ±0.85 MHz |

### B210 antenna modes

The LibreSDR B210mini reports **two receive chains** in UHD. Each chain can select **TX/RX** or **RX2**. Choose a receive mode in the toolbar. FM band scans and AM use the manual input. [Ettus B2x0 hardware notes](https://files.ettus.com/manual_archive/v4.5.0.0/html/page_usrp_b200.html)

- **Dual RX** receives through RX 0 and RX 1 simultaneously. Choose one port per channel, then press **Listen**. The app demodulates both inputs and switches the audio to the cleaner one when its advantage persists. Hover over **Using RX** for the signal level and stereo pilot SNR of both inputs. The B210 shares its receive LO, so both inputs cover the same RF window.
- **Auto RX** samples all four ports in sequence when you tune to an FM station and selects the best input. Hover over the selected input for the readings, or use **Recheck** to repeat the comparison.
- **Manual RX** lets you choose a channel and a port.

Dual RX uses 1.14 MS/s per input for a single station and up to 2.28 MS/s per input when streaming nearby stations. I tested simultaneous reception and RDS decoding at 97.00 and 102.50 MHz on my LibreSDR B210mini over USB 2.0. The same setup streamed 97.00 and 98.50 MHz together.

### Bench notes

- **UHD 4.11 compares serial numbers as hex.** My clone shipped with serial `OVXI4WE`, so it was never found when the serial was given (SDRangel always gives one). The fix is to burn a hex serial: `usrp_burn_mb_eeprom --args type=b200 --values serial=B210A1C`.
- The app budgets 4.56 MS/s total in sc16 on USB 2.0, shared across both inputs in Dual RX.
- Some RDS error correction is good. Too much is bad: correcting 5-bit bursts turned `MUSIC` into `MUSQL`.

## Build & run

```powershell
cargo build --release
target\release\rds-fm-gui.exe            # the app
target\release\rds-fm-gui.exe tune 97.0  # start on a station
target\release\rds-fm.exe devices        # list SDRs
target\release\rds-fm.exe tune 102.5 --device uhd --rx-channel 1 --antenna RX2
target\release\rds-fm.exe tune 102.5 --device uhd --rx-auto
target\release\rds-fm.exe tune 102.5 --device uhd --rx-dual --antenna RX2 --rx1-antenna TX/RX
target\release\rds-fm.exe tune 97.0 --stream 0.0.0.0:8000 --also 95.5,98.5   # headless, 3 streams
target\release\rds-fm.exe selftest       # synthetic-signal self test, no hardware needed
target\release\rds-fm.exe tmc-download   # fetch the RDS-TMC event list
```

Needs Rust (MSVC) on Windows. The encoders (LAME, FDK-AAC) are compiled from source by their crates. Close SDRangel first, because only one program can own the SDR. Settings, logbook and TMC data live in `%APPDATA%\rds-fm`.

Streams are at `http://<your-pc>:8000/` (player page), `/fm9700` (97.00 MHz), `/am891` (891 kHz). They work in browsers, VLC, foobar2000, Winamp and so on. To reach listeners outside your network, forward the port on your router or push to an Icecast server.

## RDS-TMC in Bangkok

Open **Traffic** and try **102.50 MHz** or **88.50 MHz**, the frequencies documented by iTIC. Tune to the FM carrier. RDS rides on the 57 kHz subcarrier and is extracted from the station's FM multiplex. These are listening shortcuts, with reception subject to what each station is transmitting. [iTIC Bangkok service](https://iticfoundation.org/en/about-us/how-does-itic-work/) · [iTIC FM multiplex diagram](https://org.iticfoundation.org/th/projects/realtime)

The receiver uses the station's 3A announcement to identify the ALERT-C data groups and the location table. Matching copies confirm each group. Single-group events and complete multi-group messages appear in **Traffic**. The extra multi-group payload is available as raw hex; encrypted services show their status. The CLI also exposes group counts and messages through `/api/state`. [ISO 14819-1](https://www.iso.org/standard/76781.html)

Use **Download event list** to resolve event codes. To resolve single-group location codes, use a matching iTIC table and import a normalized UTF-8 file under **Location CSV**, or keep it as `%APPDATA%\rds-fm\tmc_locations.csv` for automatic loading. Use the header `pi;ltn;location;name`, with PI in hexadecimal and LTN and location in decimal. The table is supplied separately by iTIC. Location codes are shown numerically until a matching entry is available. [iTIC Location Table](https://iticfoundation.org/location-table/) · [OTP ITS overview, p. 24](https://www.otp.go.th/uploads/files/1503313567-s2h8d-6s9lv.pdf#page=24) · [NBTC Spectrum Outlook, p. 59](https://www.nbtc.go.th/downloadfile/%E0%B8%A3%E0%B8%B2%E0%B8%A2%E0%B8%87%E0%B8%B2%E0%B8%99%E0%B9%82%E0%B8%84%E0%B8%A3%E0%B8%87%E0%B8%81%E0%B8%B2%E0%B8%A3-Spectrum-Outlook.pdf?mediaId=553765#page=59)

## How it works (the fun part)

```
scan:   burst capture ─► one big FFT ─► station finder (power centroid) ─► per-station FFT slice ─► FM discriminator ─► MPX
listen: continuous IQ ─► overlap-save FFT channelizer (N stations, one FFT, phase-continuous)
          FM: MPX 228 kHz ─┬─ 19 kHz pilot PLL ─► 38 kHz L−R ─► FIR 15 kHz ÷5 ─► de-emphasis ─► L/R
                           └─ RDS every 2.3 s
          AM: |z| ─► carrier-referenced AGC
        ─► 48 kHz ─► EQ ─► RNNoise + exciter ─┬─► recorder (WAV ─► FLAC/MP3/AAC)
                                              ├─► LAME ─► stream hub ─► HTTP listeners / Icecast
                                              └─► volume ─► drift-tracking resampler ─► WASAPI
RDS:    57 kHz baseband ─► carrier from pilot³ (or BPSK squaring) ─► biphase matched filter ─► differential decode
        ─► syndrome sync ─► ≤2-bit burst correction ─► groups 0A…15A ─► "seen twice" debounce for text
```

Every rate is a multiple of 228 kHz (12 × 19 kHz), so shifting, filtering and decimating are just picking FFT bins. Thai PS/RT text in TIS-620 is detected automatically.

## Tests

```powershell
cargo test --release
```
These cover RDS checkwords and burst correction, RT+/EON/TA/TMC parsing, channelizer phase continuity, two stations from one FFT, stereo separation (> 30 dB), AM, the resampler, EQ, RNNoise and the exciter, all four recording formats, the HTTP stream server with ICY metadata, and an end-to-end decode of synthetic stations (including Thai RadioText).

## Credits & licenses of things I didn't write
- [RNNoise](https://jmvalin.ca/demo/rnnoise/) by Jean-Marc Valin, via [`nnnoiseless`](https://crates.io/crates/nnnoiseless)
- LAME (`mp3lame-encoder`), Fraunhofer FDK AAC (`fdk-aac`), `flacenc`, `hound`, `cpal`, `egui`/`eframe`, `rustfft`
- The RDS-TMC event list is **not** bundled. The app downloads it on request from [GNU Radio gr-rds](https://github.com/bastibl/gr-rds) (GPL-2.0+), and it stays on your PC.
- UHD by Ettus Research. librtlsdr by osmocom, in the RTL-SDR Blog build.

License: not decided yet. Ask me if you want to use it.
