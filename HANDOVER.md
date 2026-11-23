# Handover — RDS FM Monitor

For the next coding agent picking this up. Read it top to bottom once; after that, the "Invariants" and "Known issues" sections are the ones to keep at hand.

## 1. What this is

A Windows desktop SDR radio in Rust (crate `rds-fm`):
- **FM band scan** that decodes the RDS of every station.
- **Live listening**: FM stereo, MW/SW/airband AM.
- **Full RDS**: RT+, EON, TMC, TA.
- **Audio**: EQ, RNNoise denoise, bandwidth-extension exciter.
- **Recording**: WAV/FLAC/MP3/AAC.
- **Presets** and **logbook**.
- **Internet streaming**: an Icecast-style server that can carry several FM stations from one capture, plus push to Icecast.

Hardware the owner has:
- **LibreSDR B210mini** (USRP B210 clone, XC7A100T + AD9361), on **USB 2.0**, driven by UHD 4.11.
- **RTL-SDR Blog V4**: supported in code but **never tested on hardware** (it was not plugged in during development).

The owner writes in Thai; reply in Thai. The UI and README are English by request.

## 2. Build, run, test

```powershell
cargo build --release
cargo test --release              # 26 tests, ~3 s, no hardware needed
target\release\rds-fm-gui.exe     # GUI. The owner runs it from dist\RDS FM Monitor.exe (a copy)
target\release\rds-fm.exe --help  # CLI + web UI (http://127.0.0.1:8765), useful for headless hardware tests
target\release\rds-fm.exe selftest
```

- Toolchain: Rust stable MSVC, VS 2026 Build Tools. LAME and FDK-AAC are compiled from C sources by their crates.
- `build.rs` embeds `assets/icon.ico` via `winresource`.
- The owner wants **an .exe, not .bat files**. After a release build, refresh `dist\` (it is git-ignored):
  `cp target/release/rds-fm-gui.exe "dist/RDS FM Monitor.exe"`.
  Check first that the app isn't running; the owner often has it open.

## 3. Code map

| File | Role |
|---|---|
| `src/uhd.rs` | UHD C API via `libloading` (no link-time dependency): open, rate/freq/gain/antenna, burst `capture`, continuous `start_stream/read/stop_stream`, `find_devices` |
| `src/rtlsdr.rs` | librtlsdr via `libloading` (`rtlsdr.dll`, RTL-SDR Blog build that ships with SDRangel) |
| `src/device.rs` | `enum Sdr { Uhd, Rtl }`, `enumerate()` (always lists the two default devices, `present` flag), freq ranges |
| `src/dsp.rs` | FFT helpers, `extract` (FFT-slice channelizer), MPX analysis, pilot-coherent RDS carrier, `rds_bits` |
| `src/pipeline.rs` | Burst scan: `detect_stations` (50 kHz grid + power centroid), `decode_station`, `band_spectrum` |
| `src/stream.rs` | Streaming overlap-save channelizer, N channels from one FFT, phase-continuous |
| `src/demod.rs` | FM discriminator, stereo decoder (pilot PLL, 38 kHz L−R, FIR ÷5, de-emphasis), AM envelope |
| `src/audio.rs` | 10-band EQ, polyphase resampler with drift adjust, cpal output (`AudioOut`) |
| `src/enhance.rs` | RNNoise (nnnoiseless) wet/dry, harmonic exciter, stereo width — 48 kHz only |
| `src/record.rs` | Recorder: WAV directly, others via temp float WAV → transcode on stop |
| `src/broadcast.rs` | `LiveMp3`, `Hub` (mount fan-out), raw-TCP HTTP stream server with ICY metadata, Icecast PUT source client |
| `src/rds.rs` | Checkword/syndrome, group sync, burst-correction table |
| `src/station.rs` | RDS application layer (PS/RT/RT+/EON/PIN/DI/Long PS/ODA/TMC hookup), text debounce |
| `src/tmc.rs` | RDS-TMC 3A/8A decoding, event-list download/parse |
| `src/text.rs` | EBU Latin table, automatic TIS-620 (Thai) detection |
| `src/app.rs` | Shared `App` state (behind `Arc<Mutex<_>>`), settings JSON, presets, logbook |
| `src/worker.rs` | Device lifecycle, FM `scan_step`, `listen_session` (multi-channel, audio, RDS, streams, in-place retune), stream-server manager |
| `src/bin/rds-fm-gui.rs` | egui 0.37 GUI (note: the 0.37 API uses `App::ui(&mut Ui)`, `egui::Panel::top/…`) |
| `src/main.rs`, `src/web.rs`, `web/index.html` | CLI and the small web dashboard (older feature set) |
| `src/synth.rs`, `tests/` | Synthetic FM+RDS generator, end-to-end and sensitivity tests |

Runtime data lives in `%APPDATA%\rds-fm\`: `settings.json` (it includes the Icecast password, so **never commit it**), `logbook.csv`, `stations.json` and `tmc_events.csv`.

## 4. Invariants (don't break these)

1. **Every IQ rate is a multiple of 228 kHz** (= 12 × 19 kHz).
   - Scan: B210 4.56 MS/s, RTL 2.28 MS/s.
   - Listen: 1.14 MS/s. Multi-station streaming uses the scan rates.
   - Channel shift, filtering and decimation are FFT-bin slices; MPX is 228 kHz with 2^19 samples per RDS block.
   - 57 kHz is exactly on the FFT grid. **19 kHz is not**: `dsp::pilot_from_spectrum` removes the fractional bin, otherwise pilot³ drifts by one bin per block.
2. **USB 2.0 budget**: stay at or below 4.56 MS/s sc16 on the B210.
3. **RDS error correction is limited to ≤2 flipped bits** with 3 clean blocks, and 1 bit with 2.
   - Correcting the full 5-bit bursts the code allows turns ~36% of garbage blocks into wrong text. It was seen on air: "MUSIC" became "MUSQL".
   - Text from corrected blocks must be seen twice before it is displayed (`Group::corrected`, `station::confirm`).
4. **Program audio is 48 kHz** after demod. EQ, enhancer, recorder and stream encoders all run at 48 kHz; only the output resampler adapts to the sound card.
5. **Channelizer phase continuity**: output is rotated by `exp(-j2π·k0·start/nfft)` per block. Test: `stream::tests`.
6. **No `println!` in code reachable from the GUI.** The release GUI is a windows-subsystem app with no stdout, and `println!` panics there. Use `writeln!(std::io::stdout(), …)` and ignore errors (`App::push_log` does this).
7. **The GPL TMC event list is never committed.** It is downloaded to `%APPDATA%` on request (`rds-fm tmc-download` or the Traffic tab).
8. **Station keys are kHz.** In scan mode a detection ±50 kHz from a known station is merged into it.
9. **Streaming only when a server or Icecast is enabled.** Otherwise no MP3 encoding happens and the "ON AIR" badge stays off.

## 5. Environment facts learned the hard way

- **UHD 4.11 compares serial numbers as hex** (`device_filter.hpp` → `serial_numbers_match` = `stoi(..,16)`). The clone shipped with serial `OVXI4WE` and never matched. The board has since been burned to `B210A1C`.
- The FPGA image in `C:\Program Files\UHD\share\uhd\images\usrp_b210_fpga.bin` is the owner's XC7A100T build (fork of lmesserStep/LibreSDRB210). It works: loopback passes, the clock goes to 48 MHz.
- **The B210 cannot tune below ~50 MHz**, so MW/SW needs the RTL-SDR V4. The worker refuses with a clear error.
- **SDRangel and this app cannot share the SDR.** Close one before opening the other.
- **Location**: Bangkok, indoors, stock antenna. Typical levels are −40…−55 dBFS at gain 50. Reliable RDS test stations:
  - 97.0 NBT (PI 2A93)
  - 106.5 GREEN (2AA6)
  - 102.5 RTAF06 (2C64)
  - 95.5 Radio Thailand (blank PS, RT only)
  - 88.0 NBT 88
- **PowerShell eats Rust panic messages** (they show as `RemoteException`). Run `cargo test` from bash to see them.
- **Hardware tests via the CLI/web API change the owner's saved settings** (volume, streams, server bind). Restore them afterwards.
- **GUI verification**:
  - Use PrintWindow screenshots of the window and the `--tab <station|rds|traffic|audio|record>` startup argument.
  - **Never use mouse/keyboard automation.** The owner uses the PC at the same time.
  - Starting the GUI in listen mode plays audio on the owner's speakers; set the volume to 0 first or warn them.

## 6. Known issues / next steps (roughly by value)

1. **Off-grid tuning creates twin stations.** Dragging to e.g. 102.55 creates a station next to 102.50. Merge listen-mode results within ±50 kHz of a known PI-matching station, or key stations by PI when known.
2. **RTL-SDR V4 is untested**: open, gain table, HF upconverter path (MW/SW), streaming read loop, sample-rate acceptance at 1.14/2.28 MS/s. The owner needs the WinUSB driver (Zadig).
3. **Git remote and push**: the repo is local only. The owner asked for commits **under their name only — no AI co-author trailers**.
4. **License is undecided** (README says so). Ask before adding one.
5. **The web UI (`web/index.html`)** lacks the newer features (bands, audio, streams, presets).
6. **Thai TMC location table**: none is public, so location codes show as numbers. The event list (1402 events) works.
7. **Recording**: FLAC keeps the encoded stream in memory until stop (long recordings use RAM), and the last <50 ms of AAC is not flushed.
8. **Stream settings changes restart the listen session** (by design, to reopen mounts). Live add/remove of channels would be nicer.
9. **The CLI saves settings only between sessions** (the GUI saves immediately).
10. **Possible features**: waterfall display, scheduled recording, AF-following, EON-based TA switching, DAB+ (no hardware for it), a proper PI→station-name database for Thailand.

## 7. Git conventions for this repo

- Branch `main`. Author is the owner (global git config); don't change `user.name` or `user.email`.
- **No `Co-Authored-By` or other AI attribution lines** in commit messages (owner's request).
- Don't commit `target/`, `dist/`, `stations.json`, `*.part.wav`, `tmc_events.csv`, or anything from `%APPDATA%\rds-fm`.
- Push only when the owner asks.
