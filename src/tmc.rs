//! RDS-TMC (ISO 14819-1) on ODA group 8A: system information from 3A and user messages.
//!
//! Event and location *names* need the ISO 14819-2 event list and the national location
//! table, which are not bundled. Optional `tmc_events.csv` (`code;text` per line) maps
//! event codes to names; otherwise codes are shown as numbers.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// FM stations documented by iTIC as carrying RDS-TMC in Bangkok.
pub const BANGKOK_STATIONS: [(u32, &str); 2] = [
    (102_500, "Royal Thai Air Force"),
    (88_500, "Royal Thai Navy"),
];

/// Local names keyed by the transmitter's PI, its location table number and the code.
pub type LocationNames = HashMap<(u16, u8, u16), String>;

/// Import normalized UTF-8 rows of the form `pi;ltn;location;name`.
pub fn load_location_names(path: &std::path::Path) -> Result<LocationNames, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    parse_location_names(&text)
}

fn parse_location_names(text: &str) -> Result<LocationNames, String> {
    let mut names = LocationNames::new();
    for (i, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.eq_ignore_ascii_case("pi;ltn;location;name") {
            continue;
        }
        let mut fields = line.splitn(4, ';').map(str::trim);
        let row = (|| {
            let pi = u16::from_str_radix(fields.next()?, 16).ok()?;
            let ltn: u8 = fields.next()?.parse().ok()?;
            let code: u16 = fields.next()?.parse().ok()?;
            let name = fields.next()?;
            (pi != 0 && (1..=63).contains(&ltn) && !name.is_empty()).then_some(((pi, ltn, code), name))
        })();
        let Some((key, name)) = row else {
            return Err(format!("Line {} must be pi;ltn;location;name (PI in hexadecimal)", i + 1));
        };
        if names.insert(key, name.to_owned()).is_some() {
            return Err(format!("Duplicate PI, LTN and location on line {}", i + 1));
        }
    }
    if names.is_empty() {
        return Err("No location names found".into());
    }
    Ok(names)
}

#[derive(Clone, Debug, Default)]
pub struct TmcSystem {
    /// Location table number (0 = encrypted service).
    pub ltn: Option<u8>,
    pub afi: Option<bool>,
    /// Mode: false = basic, true = enhanced (variant 1 timing present).
    pub enhanced: Option<bool>,
    /// Message geographic scope: International, National, Regional, Urban.
    pub scope: Option<[bool; 4]>,
    pub sid: Option<u8>,
    pub gap: Option<u8>,
}

#[derive(Clone, Debug)]
pub struct TmcMessage {
    pub ltn: u8,
    pub event: u16,
    pub location: u16,
    /// true = negative direction.
    pub direction_neg: bool,
    pub extent: u8,
    /// Duration & persistence code (single-group messages), None for multi-group.
    pub dp: Option<u8>,
    pub diversion: bool,
    pub multi_group: bool,
    /// Up to four 28-bit optional payload words, in broadcast order.
    pub optional_words: Vec<u32>,
    pub count: u32,
    pub first_seen: Instant,
    pub last_seen: Instant,
}

#[derive(Clone, Debug, Default)]
pub struct Tmc {
    pub system: TmcSystem,
    pub messages: Vec<TmcMessage>,
    pub groups: u64,
    pub tuning_groups: u64,
    previous: Option<([u16; 3], Instant)>,
    pending: [Option<PendingMessage>; 8],
}

#[derive(Clone, Debug)]
struct PendingMessage {
    message: TmcMessage,
    next_sequence: Option<u16>,
}

pub fn dp_text(dp: u8) -> &'static str {
    match dp {
        0 => "unspecified",
        1 => "~15 min",
        2 => "~30 min",
        3 => "~1 h",
        4 => "~2 h",
        5 => "~3 h",
        6 => "~4 h",
        _ => "rest of day",
    }
}

impl Tmc {
    pub fn to_json(&self) -> Value {
        let messages: Vec<_> = self.messages.iter().map(|m| json!({
            "event": m.event, "location": m.location, "direction": if m.direction_neg { "-" } else { "+" },
            "extent": m.extent, "dp": m.dp, "count": m.count, "ltn": m.ltn,
            "multi_group": m.multi_group, "optional_words": m.optional_words,
            "diversion": m.diversion, "age_s": m.last_seen.elapsed().as_secs_f32(),
        })).collect();
        json!({
            "ltn": self.system.ltn, "sid": self.system.sid,
            "groups": self.groups, "tuning_groups": self.tuning_groups,
            "messages": messages,
        })
    }

    /// 3A block C for the TMC AID.
    pub fn on_3a_message(&mut self, c: u16) {
        match c >> 14 {
            0 => {
                let ltn = ((c >> 6) & 0x3F) as u8;
                if self.system.ltn != Some(ltn) {
                    self.previous = None;
                    self.pending.fill(None);
                }
                let s = &mut self.system;
                s.ltn = Some(ltn);
                s.afi = Some((c >> 5) & 1 == 1);
                s.enhanced = Some((c >> 4) & 1 == 1);
                s.scope = Some([(c >> 3) & 1 == 1, (c >> 2) & 1 == 1, (c >> 1) & 1 == 1, c & 1 == 1]);
            }
            1 => {
                self.system.gap = Some(((c >> 12) & 3) as u8);
                self.system.sid = Some(((c >> 6) & 0x3F) as u8);
            }
            _ => {}
        }
    }

    /// One ALERT-C data group. Process a payload only after two matching copies.
    pub fn on_group(&mut self, b: u16, c: u16, d: u16) -> Option<String> {
        self.groups += 1;
        let tuning = (b >> 4) & 1 == 1;
        if tuning {
            self.tuning_groups += 1;
            return None;
        }
        let ltn = self.system.ltn.filter(|&n| n != 0)?;
        let now = Instant::now();
        let payload = [b & 0xF, c, d];
        if !self.previous.take().is_some_and(|(last, at)| last == payload && now - at <= Duration::from_secs(10)) {
            self.previous = Some((payload, now));
            return None;
        }

        let single = b & 8 != 0;
        let flag = c & 0x8000 != 0;
        if single || flag {
            let message = TmcMessage {
                ltn,
                event: c & 0x7FF,
                location: d,
                direction_neg: c & 0x4000 != 0,
                extent: ((c >> 11) & 7) as u8,
                dp: single.then_some((b & 7) as u8),
                diversion: single && flag,
                multi_group: !single,
                optional_words: Vec::new(),
                count: 1,
                first_seen: now,
                last_seen: now,
            };
            if single {
                return self.add_message(message);
            }
            self.pending[(b & 7) as usize] = Some(PendingMessage { message, next_sequence: None });
            return None;
        }

        let slot = &mut self.pending[(b & 7) as usize];
        let pending = slot.as_mut()?;
        let second = c & 0x4000 != 0;
        let sequence = (c >> 12) & 3;
        if now - pending.message.first_seen > Duration::from_secs(30) {
            *slot = None;
            return None;
        }
        if !second && pending.next_sequence != Some(sequence) {
            return None;
        }
        if second {
            pending.message.optional_words.clear();
        }
        pending.message.last_seen = now;
        pending.message.optional_words.push((u32::from(c & 0x0FFF) << 16) | u32::from(d));
        pending.next_sequence = sequence.checked_sub(1);
        if sequence == 0 {
            let message = slot.take()?.message;
            self.add_message(message)
        } else {
            None
        }
    }

    fn add_message(&mut self, message: TmcMessage) -> Option<String> {
        if let Some(m) = self.messages.iter_mut().find(|m| {
            m.ltn == message.ltn && m.event == message.event && m.location == message.location
                && m.direction_neg == message.direction_neg && m.extent == message.extent
                && m.multi_group == message.multi_group && m.optional_words == message.optional_words
        }) {
            m.count += 1;
            m.last_seen = message.last_seen;
            m.dp = message.dp;
            m.diversion = message.diversion;
            return None;
        }
        let description = format!(
            "TMC event {} at location {} ({}, extent {}, LTN {})",
            message.event, message.location, if message.direction_neg { "−" } else { "+" },
            message.extent, message.ltn,
        );
        self.messages.push(message);
        if self.messages.len() > 500 {
            self.messages.remove(0);
        }
        Some(description)
    }
}

/// Source of the event list downloaded on request: GNU Radio gr-rds `tmc_events.h`
/// (ISO 14819-2 Table 2, CEN-English texts). It is GPL-2.0-or-later, so it is fetched
/// onto the user's machine rather than shipped with this program.
pub const EVENT_LIST_URL: &str = "https://raw.githubusercontent.com/bastibl/gr-rds/maint-3.10/lib/tmc_events.h";

/// Where the downloaded/converted event list is kept.
pub fn default_event_csv() -> std::path::PathBuf {
    std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ".".into())
        .join("rds-fm")
        .join("tmc_events.csv")
}

/// Parse gr-rds `tmc_events.h` rows `{"row","text","code","quantifier"}`.
pub fn parse_gr_rds_events(src: &str) -> HashMap<u16, String> {
    let mut out = HashMap::new();
    for line in src.lines() {
        let l = line.trim();
        if !l.starts_with("{\"") {
            continue;
        }
        let fields: Vec<&str> = l.split('"').collect();
        // {"row","text","code","q"} -> ["{", row, ",", text, ",", code, ",", q, "},"]
        if fields.len() >= 8 {
            let (text, code) = (fields[3].trim(), fields[5].trim());
            if let (false, Ok(c)) = (text.is_empty(), code.parse::<u16>()) {
                out.entry(c).or_insert_with(|| text.to_string());
            }
        }
    }
    out
}

/// Download the event list and store it as `code;text` CSV. Returns the number of events.
pub fn download_event_list(dest: &std::path::Path) -> Result<usize, String> {
    let body = ureq::get(EVENT_LIST_URL)
        .call()
        .map_err(|e| format!("download failed: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let map = parse_gr_rds_events(&body);
    if map.len() < 100 {
        return Err(format!("unexpected content ({} events parsed)", map.len()));
    }
    let mut codes: Vec<_> = map.iter().collect();
    codes.sort_by_key(|(c, _)| **c);
    let mut csv = String::from(
        "# RDS-TMC ALERT-C event list (ISO 14819-2 Table 2, CEN-English)\n\
         # Source: GNU Radio gr-rds lib/tmc_events.h, GPL-2.0-or-later\n",
    );
    for (c, t) in codes {
        csv += &format!("{c};{t}\n");
    }
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    std::fs::write(dest, csv).map_err(|e| e.to_string())?;
    Ok(map.len())
}

/// Optional event list: `code;text` (or `code,text`) per line, `#` comments.
pub fn load_event_names(path: &std::path::Path) -> HashMap<u16, String> {
    let Ok(s) = std::fs::read_to_string(path) else { return HashMap::new() };
    s.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| {
            let (code, text) = l.split_once(';').or_else(|| l.split_once(','))?;
            Some((code.trim().parse().ok()?, text.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gr_rds_rows() {
        let src = r#"
static const char* tmc_events[TMC_EVENTS][4] = {
{"0"," "," "," "},
{"5","traffic problem","1"," "},
{"6","stationary traffic","101"," "},
{"7","stationary traffic for 1 km","102","4"},
"#;
        let m = parse_gr_rds_events(src);
        assert_eq!(m.len(), 3);
        assert_eq!(m[&101], "stationary traffic");
        assert_eq!(m[&102], "stationary traffic for 1 km");
    }

    #[test]
    fn single_group_message_and_system_info() {
        let mut t = Tmc::default();
        // variant 0: LTN 17, AFI=1, enhanced=0, scope I,N,R,U = 0,1,1,0
        t.on_3a_message((17 << 6) | (1 << 5) | (1 << 2) | (1 << 1));
        assert_eq!(t.system.ltn, Some(17));
        assert_eq!(t.system.afi, Some(true));
        assert_eq!(t.system.scope, Some([false, true, true, false]));
        // single group (F=1), DP=3; C: diversion=0, direction -, extent 2, event 101
        let b = (8 << 12) | (1 << 3) | 3;
        let c = (1 << 14) | (2 << 11) | 101;
        assert!(t.on_group(b, c, 12345).is_none());
        assert!(t.on_group(b, c, 12345).is_some());
        assert!(t.on_group(b, c, 12345).is_none());
        assert!(t.on_group(b, c, 12345).is_none()); // confirmed repeat
        let m = &t.messages[0];
        assert_eq!((m.event, m.location, m.direction_neg, m.extent, m.dp, m.count), (101, 12345, true, 2, Some(3), 2));
        // tuning information groups are counted separately
        t.on_group((8 << 12) | (1 << 4), 0, 0);
        assert_eq!(t.tuning_groups, 1);
    }

    #[test]
    fn assembles_multigroup_payload_by_continuity_and_sequence() {
        let mut t = Tmc::default();
        t.on_3a_message(17 << 6);
        let mut pair = |b, c, d| {
            assert!(t.on_group(b, c, d).is_none());
            t.on_group(b, c, d)
        };
        let b = (8 << 12) | 2;
        assert!(pair(b, 0x8000 | 101, 12345).is_none());
        assert!(pair(b + 1, 0x4001, 0xCAFE).is_none()); // different continuity index
        assert!(pair(b, 0x0001, 0x1234).is_none()); // second group still missing
        assert!(pair(b, 0x6002, 0x5678).is_none()); // second group, sequence 2
        assert!(pair(b, 0x0003, 0x9ABC).is_none()); // skip sequence 1
        assert!(pair(b, 0x1004, 0xDEF0).is_none());
        assert!(pair(b, 0x0005, 0x1234).is_some());
        assert_eq!(t.messages.len(), 1);
        let m = &t.messages[0];
        assert_eq!((m.event, m.location, m.ltn, m.multi_group), (101, 12345, 17, true));
        assert_eq!(m.optional_words, [0x25678, 0x4DEF0, 0x51234]);
    }

    #[test]
    fn location_names_use_pi_and_ltn() {
        let names = parse_location_names("\u{feff}pi;ltn;location;name\n2C64;17;12345;สี่แยก\n2C64;18;12345;ถนน\n").unwrap();
        assert_eq!(names[&(0x2C64, 17, 12345)], "สี่แยก");
        assert_eq!(names[&(0x2C64, 18, 12345)], "ถนน");
        assert!(parse_location_names("2C64;0;12345;Encrypted").is_err());
        assert!(parse_location_names("2C64;17;12345;A\n2C64;17;12345;B").is_err());
    }
}
