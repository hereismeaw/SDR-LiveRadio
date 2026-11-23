//! RDS-TMC (ISO 14819-1) on ODA group 8A: system information from 3A and user messages.
//!
//! Event and location *names* need the ISO 14819-2 event list and the national location
//! table, which are not bundled. Optional `tmc_events.csv` (`code;text` per line) maps
//! event codes to names; otherwise codes are shown as numbers.

use std::collections::HashMap;
use std::time::Instant;

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
    pub event: u16,
    pub location: u16,
    /// true = negative direction.
    pub direction_neg: bool,
    pub extent: u8,
    /// Duration & persistence code (single-group messages), None for multi-group.
    pub dp: Option<u8>,
    pub diversion: bool,
    pub multi_group: bool,
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
    /// 3A block C for the TMC AID.
    pub fn on_3a_message(&mut self, c: u16) {
        match c >> 14 {
            0 => {
                let s = &mut self.system;
                s.ltn = Some(((c >> 6) & 0x3F) as u8);
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

    /// One 8A group. Returns a description when a new message appears.
    pub fn on_group(&mut self, b: u16, c: u16, d: u16) -> Option<String> {
        self.groups += 1;
        let tuning = (b >> 4) & 1 == 1;
        if tuning {
            self.tuning_groups += 1;
            return None;
        }
        let single = (b >> 3) & 1 == 1;
        if !single && (c >> 15) & 1 == 0 {
            return None; // subsequent group of a multi-group message (free format)
        }
        let event = c & 0x7FF;
        let location = d;
        let direction_neg = (c >> 14) & 1 == 1;
        let extent = ((c >> 11) & 7) as u8;
        let dp = single.then_some((b & 7) as u8);
        let diversion = single && (c >> 15) & 1 == 1;
        let now = Instant::now();
        if let Some(m) = self.messages.iter_mut().find(|m| {
            m.event == event && m.location == location && m.direction_neg == direction_neg && m.extent == extent
        }) {
            m.count += 1;
            m.last_seen = now;
            m.dp = dp.or(m.dp);
            return None;
        }
        self.messages.push(TmcMessage {
            event,
            location,
            direction_neg,
            extent,
            dp,
            diversion,
            multi_group: !single,
            count: 1,
            first_seen: now,
            last_seen: now,
        });
        if self.messages.len() > 500 {
            self.messages.remove(0);
        }
        Some(format!(
            "TMC event {event} at location {location} ({}, extent {extent})",
            if direction_neg { "−" } else { "+" }
        ))
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
        assert!(t.on_group(b, c, 12345).is_some());
        assert!(t.on_group(b, c, 12345).is_none()); // repeat -> counted, not new
        let m = &t.messages[0];
        assert_eq!((m.event, m.location, m.direction_neg, m.extent, m.dp, m.count), (101, 12345, true, 2, Some(3), 2));
        // tuning information groups are counted separately
        t.on_group((8 << 12) | (1 << 4), 0, 0);
        assert_eq!(t.tuning_groups, 1);
    }
}
