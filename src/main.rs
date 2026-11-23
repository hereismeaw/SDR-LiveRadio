mod web;

use rds_fm::app::{App, Mode};
use rds_fm::device::{self, DriverPaths};
use rds_fm::worker;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const DEFAULT_IMAGES: &str = r"C:\Program Files\UHD\share\uhd\images";

fn usage() {
    println!(
        "rds-fm — FM RDS receiver/decoder and radio for USRP B2xx (UHD) and RTL-SDR

Usage:
  rds-fm [scan] [options]        scan 87.5–108 MHz continuously, results in the web page
  rds-fm tune <MHz> [options]    listen to one frequency (audio + RDS), e.g. rds-fm tune 95.5
                                 (MW/SW in MHz too, e.g. 0.891 or 9.39, needs the RTL-SDR)
  rds-fm devices                 list the SDRs that are found
  rds-fm tmc-download            fetch the RDS-TMC event list (gr-rds, GPL-2.0+) into %APPDATA%\rds-fm
  rds-fm selftest                test DSP/decoder with a synthetic signal (no hardware)

Options:
  --device uhd|rtl   which SDR to use (default: last used / first found)
  --gain <dB>        RX gain (default 30); --agc for automatic gain (RTL-SDR)
  --antenna <name>   TX/RX or RX2 (UHD only, default TX/RX)
  --rx-channel <n>  UHD receive channel 0 or 1 (default 0 on a new install)
  --rx-auto         Compare UHD antenna inputs when tuning to FM
  --rx-dual         Receive FM on both B210 channels at the same time
  --rx1-antenna <name>  RX 1 port in Dual RX (default RX2); --antenna sets RX 0
  --port <n>         web UI port (default 8765)
  --no-open          do not open the browser
  --uhd-dll <path>   path to uhd.dll
  --rtl-dll <path>   path to rtlsdr.dll (default: next to the exe, else SDRangel's)
  --save <path>      results file (default stations.json)
  --stream <addr>    serve internet radio streams, e.g. --stream 0.0.0.0:8000
  --also <MHz,...>   extra FM stations to stream from the same capture, e.g. --also 95.5,98.5"
    );
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}\n(see: rds-fm --help)");
    std::process::exit(2);
}

fn main() {
    let mut cmd = String::from("scan");
    let mut tune_mhz: Option<f64> = None;
    let mut gain: Option<f64> = None;
    let mut agc = false;
    let mut antenna: Option<String> = None;
    let mut rx_channel: Option<usize> = None;
    let mut rx_auto = false;
    let mut rx_dual = false;
    let mut rx1_antenna = None;
    let mut want_dev: Option<String> = None;
    let mut port: u16 = 8765;
    let mut open_browser = true;
    let mut paths = DriverPaths::default();
    let mut save_path = PathBuf::from("stations.json");
    let mut stream_bind: Option<String> = None;
    let mut also: Vec<f64> = Vec::new();

    let mut it = std::env::args().skip(1);
    let mut positional = 0;
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().unwrap_or_else(|| die(&format!("{name} needs a value")));
        match a.as_str() {
            "-h" | "--help" => {
                usage();
                return;
            }
            "--gain" => gain = Some(val("--gain").parse().unwrap_or_else(|_| die("invalid --gain"))),
            "--agc" => agc = true,
            "--antenna" => antenna = Some(val("--antenna")),
            "--rx-channel" => {
                let n = val("--rx-channel").parse::<usize>().ok().filter(|&n| n <= 1);
                rx_channel = Some(n.unwrap_or_else(|| die("--rx-channel must be 0 or 1")));
            }
            "--rx-auto" => rx_auto = true,
            "--rx-dual" => rx_dual = true,
            "--rx1-antenna" => {
                let port = val("--rx1-antenna");
                if !matches!(port.as_str(), "TX/RX" | "RX2") {
                    die("--rx1-antenna must be TX/RX or RX2");
                }
                rx1_antenna = Some(port);
            }
            "--device" => want_dev = Some(val("--device")),
            "--port" => port = val("--port").parse().unwrap_or_else(|_| die("invalid --port")),
            "--uhd-dll" => paths.uhd_dll = PathBuf::from(val("--uhd-dll")),
            "--rtl-dll" => paths.rtl_dll = PathBuf::from(val("--rtl-dll")),
            "--save" => save_path = PathBuf::from(val("--save")),
            "--stream" => stream_bind = Some(val("--stream")),
            "--also" => also = val("--also").split(',').filter_map(|s| s.trim().parse::<f64>().ok()).collect(),
            "--no-open" => open_browser = false,
            s if s.starts_with("--") => die(&format!("unknown option {s}")),
            s => {
                match positional {
                    0 => cmd = s.to_string(),
                    1 if cmd == "tune" => tune_mhz = Some(s.parse().unwrap_or_else(|_| die("invalid frequency"))),
                    _ => die(&format!("unexpected argument: {s}")),
                }
                positional += 1;
            }
        }
    }
    if std::env::var_os("UHD_IMAGES_DIR").is_none() && std::path::Path::new(DEFAULT_IMAGES).exists() {
        std::env::set_var("UHD_IMAGES_DIR", DEFAULT_IMAGES);
    }

    let mode = match cmd.as_str() {
        "selftest" => match rds_fm::synth::selftest() {
            Ok(report) => {
                println!("{report}");
                return;
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
        "tmc-download" => {
            let dest = rds_fm::tmc::default_event_csv();
            match rds_fm::tmc::download_event_list(&dest) {
                Ok(n) => println!("{n} TMC event names saved to {}", dest.display()),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        "devices" => {
            let (list, notes) = device::enumerate(&paths);
            for d in list {
                println!("{} {:<14} {}", if d.present { "●" } else { "○" }, d.id, d.label);
            }
            for n in notes {
                println!("  note: {n}");
            }
            return;
        }
        "scan" => Mode::Scan,
        "tune" => {
            let f = tune_mhz.unwrap_or_else(|| die("tune needs a frequency, e.g. rds-fm tune 95.5"));
            if !(0.1..=6000.0).contains(&f) {
                die("frequency must be given in MHz");
            }
            Mode::Idle // set below via App::listen
        }
        other => die(&format!("unknown command {other}")),
    };

    let mut a = App::new(mode, 30.0, "TX/RX".into());
    a.load_settings_file();
    if rx_auto && rx_dual {
        die("choose --rx-auto or --rx-dual");
    }
    if rx_auto || rx_dual || rx_channel.is_some() || antenna.is_some() {
        a.rx_auto = rx_auto;
        a.rx_dual = rx_dual;
    }
    if let Some(port) = rx1_antenna {
        a.rx1_antenna = port;
    }
    if let Some(channel) = rx_channel {
        a.rx_channel = channel;
    }
    if let Some(g) = gain {
        a.gain = g;
    }
    a.agc = agc || (a.agc && gain.is_none());
    if let Some(ant) = antenna {
        if ant != "TX/RX" && ant != "RX2" {
            die("--antenna must be TX/RX or RX2");
        }
        a.antenna = ant;
    }
    if let Some(d) = want_dev {
        a.device_id = if d.starts_with("rtl") { "rtl:default".into() } else { "uhd:default".into() };
    }
    if let Some(f) = tune_mhz {
        a.listen(f * 1e6);
    }
    if let Some(b) = stream_bind {
        a.stream.server = true;
        a.stream.bind = b;
        a.stream.stream_main = true;
        a.streams = also.iter().map(|f| rds_fm::app::StreamCfg { freq_hz: f * 1e6, enabled: true }).collect();
    }
    let app = Arc::new(Mutex::new(a));
    {
        let app = app.clone();
        std::thread::spawn(move || worker::run_stream_server(app));
    }
    if let Err(e) = web::serve(app.clone(), port) {
        die(&e);
    }
    let url = format!("http://127.0.0.1:{port}/");
    app.lock().unwrap().push_log(format!("Web UI: {url}"));
    if open_browser {
        let _ = std::process::Command::new("cmd").args(["/C", "start", "", &url]).spawn();
    }
    worker::run(app, worker::WorkerCfg { paths, save_path });
}
