//! Per-station RDS application layer.
//!
//! Groups: 0A/0B (PS, TA, M/S, DI, AF), 1A (PIN, ECC, language, TMC id, EWS id),
//! 2A/2B (RadioText), 3A (ODA: RT+, eRT, TMC), 4A (clock), 8A (TMC), 10A (PTYN),
//! 14A/14B (EON), 15A (Long PS), plus the ODA groups announced in 3A.

use crate::rds::{group_label, BlockStats, Group};
use crate::text;
use crate::tmc::Tmc;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::time::Instant;

pub const PTY_NAMES: [&str; 32] = [
    "None", "News", "Current Affairs", "Information", "Sport", "Education", "Drama", "Culture",
    "Science", "Varied", "Pop Music", "Rock Music", "Easy Listening", "Light Classical",
    "Serious Classical", "Other Music", "Weather", "Finance", "Children's", "Social Affairs",
    "Religion", "Phone-in", "Travel", "Leisure", "Jazz Music", "Country Music", "National Music",
    "Oldies Music", "Folk Music", "Documentary", "Alarm Test", "Alarm",
];

pub const AID_RTPLUS: u16 = 0x4BD7;
pub const AID_ERT: u16 = 0x6552;
pub const AID_TMC: [u16; 2] = [0xCD46, 0xCD47];

pub fn oda_name(aid: u16) -> &'static str {
    match aid {
        0x4BD7 => "RadioText+ (RT+)",
        0x4BD8 => "RT+ for eRT",
        0x6552 => "Enhanced RadioText (eRT)",
        0xCD46 | 0xCD47 => "Traffic Message Channel (TMC)",
        0x0D45 => "TMC (test)",
        0x0000 => "-",
        _ => "unknown",
    }
}

/// RT+ content types (IEC 62106 / RT+ specification).
pub fn rtplus_name(t: u8) -> String {
    const N: [&str; 54] = [
        "DUMMY", "ITEM.TITLE", "ITEM.ALBUM", "ITEM.TRACKNUMBER", "ITEM.ARTIST", "ITEM.COMPOSITION",
        "ITEM.MOVEMENT", "ITEM.CONDUCTOR", "ITEM.COMPOSER", "ITEM.BAND", "ITEM.COMMENT", "ITEM.GENRE",
        "INFO.NEWS", "INFO.NEWS.LOCAL", "INFO.STOCKMARKET", "INFO.SPORT", "INFO.LOTTERY", "INFO.HOROSCOPE",
        "INFO.DAILY_DIVERSION", "INFO.HEALTH", "INFO.EVENT", "INFO.SCENE", "INFO.CINEMA",
        "INFO.STUPIDITY_MACHINE", "INFO.DATE_TIME", "INFO.WEATHER", "INFO.TRAFFIC", "INFO.ALARM",
        "INFO.ADVERTISEMENT", "INFO.URL", "INFO.OTHER", "STATIONNAME.SHORT", "STATIONNAME.LONG",
        "PROGRAMME.NOW", "PROGRAMME.NEXT", "PROGRAMME.PART", "PROGRAMME.HOST", "PROGRAMME.EDITORIAL_STAFF",
        "PROGRAMME.FREQUENCY", "PROGRAMME.HOMEPAGE", "PROGRAMME.SUBCHANNEL", "PHONE.HOTLINE",
        "PHONE.STUDIO", "PHONE.OTHER", "SMS.STUDIO", "SMS.OTHER", "EMAIL.HOTLINE", "EMAIL.STUDIO",
        "EMAIL.OTHER", "MMS.OTHER", "CHAT", "CHAT.CENTRE", "VOTE.QUESTION", "VOTE.CENTRE",
    ];
    match t {
        0..=53 => N[t as usize].to_string(),
        59 => "PLACE".into(),
        60 => "APPOINTMENT".into(),
        61 => "IDENTIFIER".into(),
        62 => "PURCHASE".into(),
        63 => "GET_DATA".into(),
        _ => format!("type {t}"),
    }
}

/// PI code: country nibble, coverage area, programme reference.
pub fn pi_info(pi: u16) -> String {
    const AREA: [&str; 4] = ["Local", "International", "National", "Supra-regional"];
    let area = (pi >> 8) & 0xF;
    let area = if area < 4 { AREA[area as usize].to_string() } else { format!("Regional {}", area - 3) };
    format!("country {:X} · {area} · programme {}", pi >> 12, pi & 0xFF)
}

/// Modified Julian Date -> (year, month, day), per IEC 62106 Annex G.
pub fn mjd_to_ymd(mjd: i64) -> (i64, i64, i64) {
    let m = mjd as f64;
    let yp = ((m - 15078.2) / 365.25).floor();
    let mp = ((m - 14956.1 - (yp * 365.25).floor()) / 30.6001).floor();
    let d = mjd - 14956 - (yp * 365.25).floor() as i64 - (mp * 30.6001).floor() as i64;
    let k = if mp == 14.0 || mp == 15.0 { 1 } else { 0 };
    (yp as i64 + k + 1900, mp as i64 - 1 - k * 12, d)
}

fn decode_ct(b: u16, c: u16, d: u16) -> Option<String> {
    let mjd = (((b & 3) as i64) << 15) | (c >> 1) as i64;
    let hour = (((c & 1) as i64) << 4) | (d >> 12) as i64;
    let minute = ((d >> 6) & 0x3F) as i64;
    let negative = (d >> 5) & 1 == 1;
    let off = (d & 0x1F) as i64; // half hours
    if hour > 23 || minute > 59 || mjd < 40_000 {
        return None;
    }
    let local = hour * 60 + minute + if negative { -off * 30 } else { off * 30 };
    let (y, mo, da) = mjd_to_ymd(mjd + local.div_euclid(1440));
    let lm = local.rem_euclid(1440);
    Some(format!(
        "{y:04}-{mo:02}-{da:02} {:02}:{:02} (UTC{}{:02}:{:02})",
        lm / 60,
        lm % 60,
        if negative { '-' } else { '+' },
        off / 2,
        (off % 2) * 30
    ))
}

/// Debounce a text segment: accept at once if the segment is new or unchanged,
/// otherwise only when the same new value arrives twice in a row. This stops a block
/// that slipped through the checkword from corrupting text that is already established.
fn confirm(cur: &mut [u8], known: bool, pending: &mut Option<[u8; 4]>, new: &[u8]) -> bool {
    let mut key = [0u8; 4];
    key[..new.len()].copy_from_slice(new);
    if !known || cur == new || *pending == Some(key) {
        cur.copy_from_slice(new);
        *pending = None;
        true
    } else {
        *pending = Some(key);
        false
    }
}

fn bytes4(c: u16, d: u16) -> [u8; 4] {
    [(c >> 8) as u8, c as u8, (d >> 8) as u8, d as u8]
}

/// Other network (EON, groups 14A/14B).
#[derive(Clone, Debug)]
pub struct Eon {
    ps_buf: [u8; 8],
    ps_mask: u8,
    pub tp: Option<bool>,
    pub ta: Option<bool>,
    pub pty: Option<u8>,
    pub af: BTreeSet<u16>,
    pub last: Instant,
}

impl Eon {
    fn new() -> Self {
        Self { ps_buf: [b' '; 8], ps_mask: 0, tp: None, ta: None, pty: None, af: BTreeSet::new(), last: Instant::now() }
    }
    pub fn ps(&self) -> Option<String> {
        (self.ps_mask != 0).then(|| text::decode(&Station::masked(&self.ps_buf, self.ps_mask as u32, 2)).0)
    }
}

#[derive(Clone, Debug, Default)]
pub struct RtPlus {
    pub item_toggle: Option<bool>,
    pub item_running: bool,
    /// content type -> text
    pub tags: BTreeMap<u8, String>,
}

#[derive(Clone, Debug)]
pub struct RawGroup {
    pub at: Instant,
    pub blocks: [Option<u16>; 4],
}

pub struct Station {
    pub freq_khz: u32,
    pi_votes: HashMap<u16, u32>,
    pub pi: Option<u16>,
    ps_buf: [u8; 8],
    ps_mask: u8,
    ps_known: u8,
    ps_pending: [Option<[u8; 4]>; 4],
    pub ps: Option<Vec<u8>>,
    pub ps_history: VecDeque<Vec<u8>>,
    rt_buf: [u8; 64],
    rt_mask: u16,
    rt_known: u16,
    rt_pending: [Option<[u8; 4]>; 16],
    rt_ab: Option<u8>,
    rt_seg_len: usize,
    pub rt: Option<Vec<u8>>,
    pub rt_history: VecDeque<Vec<u8>>,
    ptyn_buf: [u8; 8],
    ptyn_mask: u8,
    pub ptyn: Option<Vec<u8>>,
    pub pty: Option<u8>,
    pub tp: Option<bool>,
    pub ta: Option<bool>,
    pub music: Option<bool>,
    /// Decoder identification bits d3..d0 and which of them were received.
    pub di: u8,
    pub di_mask: u8,
    pub af: BTreeSet<u16>, // 0.1 MHz units
    pub ecc: Option<u8>,
    pub lang: Option<u8>,
    pub tmc_id: Option<u16>,
    pub ews_id: Option<u16>,
    pub pin: Option<(u8, u8, u8)>, // day, hour, minute
    pub ct: Option<String>,
    pub oda: BTreeMap<u16, String>,
    rtplus_group: Option<(u8, bool)>,
    pub rtplus: RtPlus,
    ert_group: Option<u8>,
    ert_utf8: bool,
    ert_buf: [u8; 128],
    ert_mask: u32,
    pub ert: Option<String>,
    long_ps_buf: [u8; 32],
    long_ps_mask: u8,
    pub long_ps: Option<String>,
    pub eon: BTreeMap<u16, Eon>,
    tmc_group: Option<(u16, u8)>,
    pub tmc: Tmc,
    pub raw: VecDeque<RawGroup>,
    pub group_counts: BTreeMap<String, u64>,
    pub level_db: f32,
    pub pilot_db: f32,
    pub rds_snr_db: f32,
    pub quality: f32,
    pub blocks_ok: u64,
    pub blocks_total: u64,
    pub blocks_corrected: u64,
    pub groups_total: u64,
    pub captures: u64,
    pub last_seen: Instant,
    pub last_rds: Option<Instant>,
    pub ta_since: Option<Instant>,
}

impl Station {
    pub fn new(freq_khz: u32) -> Self {
        Self {
            freq_khz,
            pi_votes: HashMap::new(),
            pi: None,
            ps_buf: [b' '; 8],
            ps_mask: 0,
            ps_known: 0,
            ps_pending: [None; 4],
            ps: None,
            ps_history: VecDeque::new(),
            rt_buf: [b' '; 64],
            rt_mask: 0,
            rt_known: 0,
            rt_pending: [None; 16],
            rt_ab: None,
            rt_seg_len: 4,
            rt: None,
            rt_history: VecDeque::new(),
            ptyn_buf: [b' '; 8],
            ptyn_mask: 0,
            ptyn: None,
            pty: None,
            tp: None,
            ta: None,
            music: None,
            di: 0,
            di_mask: 0,
            af: BTreeSet::new(),
            ecc: None,
            lang: None,
            tmc_id: None,
            ews_id: None,
            pin: None,
            ct: None,
            oda: BTreeMap::new(),
            rtplus_group: None,
            rtplus: RtPlus::default(),
            ert_group: None,
            ert_utf8: true,
            ert_buf: [0; 128],
            ert_mask: 0,
            ert: None,
            long_ps_buf: [b' '; 32],
            long_ps_mask: 0,
            long_ps: None,
            eon: BTreeMap::new(),
            tmc_group: None,
            tmc: Tmc::default(),
            raw: VecDeque::new(),
            group_counts: BTreeMap::new(),
            level_db: f32::NAN,
            pilot_db: f32::NAN,
            rds_snr_db: f32::NAN,
            quality: 0.0,
            blocks_ok: 0,
            blocks_total: 0,
            blocks_corrected: 0,
            groups_total: 0,
            captures: 0,
            last_seen: Instant::now(),
            last_rds: None,
            ta_since: None,
        }
    }

    pub fn update_metrics(&mut self, level_db: f32, pilot_db: f32, rds_snr_db: f32, stats: BlockStats) {
        self.level_db = level_db;
        self.pilot_db = pilot_db;
        self.rds_snr_db = rds_snr_db;
        self.quality = if stats.total == 0 { 0.0 } else { stats.ok as f32 / stats.total as f32 };
        self.blocks_ok += stats.ok as u64;
        self.blocks_total += stats.total as u64;
        self.blocks_corrected += stats.corrected as u64;
        self.captures += 1;
        self.last_seen = Instant::now();
    }

    pub fn has_rds(&self) -> bool {
        self.groups_total > 0
    }

    pub fn tmc_service_pi(&self) -> Option<u16> {
        self.tmc_group.map(|(pi, _)| pi)
    }

    /// Traffic programme currently broadcasting a traffic announcement.
    pub fn ta_active(&self) -> bool {
        self.tp == Some(true) && self.ta == Some(true)
    }

    fn rt_complete(&self) -> Option<Vec<u8>> {
        let sl = self.rt_seg_len;
        let max = 16 * sl;
        let end = self.rt_buf[..max].iter().position(|&x| x == 0x0D);
        let last_seg = end.map(|p| p / sl).unwrap_or(15);
        let need: u32 = (1u32 << (last_seg + 1)) - 1;
        if (self.rt_mask as u32) & need != need {
            return None;
        }
        let mut v = self.rt_buf[..end.unwrap_or(max)].to_vec();
        while v.last() == Some(&b' ') {
            v.pop();
        }
        Some(v)
    }

    fn masked(buf: &[u8], mask: u32, seg_len: usize) -> Vec<u8> {
        buf.iter()
            .enumerate()
            .map(|(i, &b)| if mask & (1 << (i / seg_len)) != 0 { b } else { b'_' })
            .collect()
    }

    /// PS for display: the confirmed PS, else every character ever received
    /// (`_` only for segments never seen).
    pub fn ps_partial(&self) -> Vec<u8> {
        Self::masked(&self.ps_buf, self.ps_known as u32, 2)
    }

    pub fn rt_partial(&self) -> Vec<u8> {
        let max = 16 * self.rt_seg_len;
        let mut v = Self::masked(&self.rt_buf[..max], self.rt_known as u32, self.rt_seg_len);
        if let Some(p) = v.iter().position(|&x| x == 0x0D) {
            v.truncate(p);
        }
        while v.last() == Some(&b'_') || v.last() == Some(&b' ') {
            v.pop();
        }
        v
    }

    /// Best PS text and whether it is a confirmed complete PS.
    pub fn ps_display(&self) -> (String, bool) {
        match &self.ps {
            Some(p) if p.iter().all(|&b| b == b' ') => ("(blank PS)".into(), true),
            Some(p) => (text::decode(p).0, true),
            None if self.ps_known != 0 => (text::decode(&self.ps_partial()).0, false),
            None => (String::new(), false),
        }
    }

    pub fn rt_display(&self) -> (String, bool) {
        match &self.rt {
            Some(r) => (text::decode(r).0, true),
            None => (text::decode(&self.rt_partial()).0, false),
        }
    }

    fn update_ert(&mut self) {
        let end = self.ert_buf.iter().position(|&x| x == 0x0D).unwrap_or(128);
        let segs = end.div_ceil(4).max(1);
        let need: u64 = (1u64 << segs) - 1;
        if (self.ert_mask as u64) & need != need {
            return;
        }
        let bytes = &self.ert_buf[..end];
        let s = if self.ert_utf8 {
            String::from_utf8_lossy(bytes).into_owned()
        } else {
            let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            char::decode_utf16(units).map(|r| r.unwrap_or('\u{FFFD}')).collect()
        };
        self.ert = Some(s.trim_end_matches(['\0', ' ']).to_string());
    }

    fn update_long_ps(&mut self) {
        let end = self.long_ps_buf.iter().position(|&x| x == 0x0D || x == 0).unwrap_or(32);
        let segs = end.div_ceil(4).max(1);
        let need: u16 = (1u16 << segs) - 1;
        if (self.long_ps_mask as u16) & need == need {
            let s = String::from_utf8_lossy(&self.long_ps_buf[..end]).trim_end().to_string();
            self.long_ps = Some(s);
        }
    }

    fn apply_rtplus(&mut self, b: u16, c: u16, d: u16, ev: &mut Vec<String>) {
        let toggle = (b >> 4) & 1 == 1;
        if self.rtplus.item_toggle != Some(toggle) {
            self.rtplus.item_toggle = Some(toggle);
            self.rtplus.tags.clear();
        }
        self.rtplus.item_running = (b >> 3) & 1 == 1;
        let t1 = (((b & 7) << 3) | (c >> 13)) as u8;
        let s1 = ((c >> 7) & 0x3F) as usize;
        let l1 = ((c >> 1) & 0x3F) as usize;
        let t2 = (((c & 1) << 5) | (d >> 11)) as u8;
        let s2 = ((d >> 5) & 0x3F) as usize;
        let l2 = (d & 0x1F) as usize;
        let rt = match &self.rt {
            Some(r) => r.clone(),
            None => return,
        };
        for (t, s, l) in [(t1, s1, l1), (t2, s2, l2)] {
            if t == 0 || s >= rt.len() {
                continue;
            }
            let end = (s + l + 1).min(rt.len());
            let txt = text::decode(&rt[s..end]).0.trim().to_string();
            if txt.is_empty() {
                continue;
            }
            if self.rtplus.tags.get(&t) != Some(&txt) {
                if matches!(t, 1 | 4) {
                    ev.push(format!("RT+ {} \"{txt}\"", rtplus_name(t)));
                }
                self.rtplus.tags.insert(t, txt);
            }
        }
    }

    fn apply_eon(&mut self, b: u16, c: Option<u16>, d: u16, ver_b: bool, ev: &mut Vec<String>) {
        let e = self.eon.entry(d).or_insert_with(Eon::new);
        e.last = Instant::now();
        e.tp = Some((b >> 4) & 1 == 1);
        if ver_b {
            let ta = (b >> 3) & 1 == 1;
            if ta && e.ta != Some(true) {
                ev.push(format!("EON traffic announcement on {} ({d:04X})", e.ps().unwrap_or_default().trim()));
            }
            e.ta = Some(ta);
            return;
        }
        let Some(c) = c else { return };
        match b & 0xF {
            v @ 0..=3 => {
                let i = v as usize * 2;
                e.ps_buf[i] = (c >> 8) as u8;
                e.ps_buf[i + 1] = c as u8;
                e.ps_mask |= 1 << v;
            }
            4 => {
                for code in [(c >> 8) as u8, c as u8] {
                    if (1..=204).contains(&code) {
                        e.af.insert(875 + code as u16);
                    }
                }
            }
            5..=8 => {
                let code = c as u8; // mapped frequency of the other network
                if (1..=204).contains(&code) {
                    e.af.insert(875 + code as u16);
                }
            }
            13 => {
                e.pty = Some((c >> 11) as u8);
                let ta = c & 1 == 1;
                if ta && e.ta != Some(true) {
                    ev.push(format!("EON traffic announcement on {} ({d:04X})", e.ps().unwrap_or_default().trim()));
                }
                e.ta = Some(ta);
            }
            _ => {}
        }
    }

    /// Apply one decoded group. Returns human-readable events (new PS / RT / TA ...).
    pub fn apply_group(&mut self, g: &Group) -> Vec<String> {
        let mut ev = Vec::new();
        let Some(b) = g.blocks[1] else { return ev };
        let (c, d) = (g.blocks[2], g.blocks[3]);

        self.raw.push_back(RawGroup { at: Instant::now(), blocks: g.blocks });
        while self.raw.len() > 200 {
            self.raw.pop_front();
        }
        if let Some(a) = g.blocks[0] {
            *self.pi_votes.entry(a).or_insert(0) += 1;
            self.pi = self.pi_votes.iter().max_by_key(|(_, n)| **n).map(|(p, _)| *p);
        }
        let gtype = (b >> 12) as u8;
        let ver_b = (b >> 11) & 1 == 1;
        self.groups_total += 1;
        self.last_rds = Some(Instant::now());
        *self.group_counts.entry(group_label(gtype, ver_b)).or_insert(0) += 1;
        self.tp = Some((b >> 10) & 1 == 1);
        self.pty = Some(((b >> 5) & 0x1F) as u8);

        match (gtype, ver_b) {
            (0, _) => {
                let ta = (b >> 4) & 1 == 1;
                if self.tp == Some(true) && ta != (self.ta == Some(true)) {
                    ev.push(if ta { "Traffic announcement started (TA)".into() } else { "Traffic announcement ended".into() });
                    self.ta_since = ta.then(Instant::now);
                }
                self.ta = Some(ta);
                self.music = Some((b >> 3) & 1 == 1);
                let seg = (b & 3) as usize;
                let di_bit = 3 - seg as u8; // seg 0 carries d3 ... seg 3 carries d0
                if (b >> 2) & 1 == 1 {
                    self.di |= 1 << di_bit;
                } else {
                    self.di &= !(1 << di_bit);
                }
                self.di_mask |= 1 << di_bit;
                if !ver_b {
                    if let Some(c) = c {
                        for code in [(c >> 8) as u8, c as u8] {
                            if (1..=204).contains(&code) {
                                self.af.insert(875 + code as u16);
                            }
                        }
                    }
                }
                if let Some(d) = d {
                    if seg == 0 {
                        self.ps_mask = 0;
                    }
                    // data from a corrected block must be seen twice before it is shown
                    let known = self.ps_known & (1 << seg) != 0 || g.corrected[3];
                    let new = [(d >> 8) as u8, d as u8];
                    if confirm(&mut self.ps_buf[seg * 2..seg * 2 + 2], known, &mut self.ps_pending[seg], &new) {
                        self.ps_mask |= 1 << seg;
                        self.ps_known |= 1 << seg;
                    }
                    if self.ps_mask == 0x0F {
                        let v = self.ps_buf.to_vec();
                        if self.ps.as_ref() != Some(&v) {
                            ev.push(format!("PS \"{}\"", text::decode(&v).0));
                            if self.ps_history.back() != Some(&v) {
                                self.ps_history.push_back(v.clone());
                                if self.ps_history.len() > 16 {
                                    self.ps_history.pop_front();
                                }
                            }
                            self.ps = Some(v);
                        }
                    }
                }
            }
            (1, false) => {
                if let Some(c) = c {
                    match (c >> 12) & 7 {
                        0 => self.ecc = Some(c as u8),
                        1 => self.tmc_id = Some(c & 0xFFF),
                        3 => self.lang = Some(c as u8),
                        7 => self.ews_id = Some(c & 0xFFF),
                        _ => {}
                    }
                }
                if let Some(d) = d {
                    let day = (d >> 11) as u8;
                    if day != 0 {
                        self.pin = Some((day, ((d >> 6) & 0x1F) as u8, (d & 0x3F) as u8));
                    }
                }
            }
            (2, _) => {
                let ab = ((b >> 4) & 1) as u8;
                let seg = (b & 0x0F) as usize;
                let seg_len = if ver_b { 2 } else { 4 };
                if self.rt_ab != Some(ab) || self.rt_seg_len != seg_len {
                    self.rt_ab = Some(ab);
                    self.rt_seg_len = seg_len;
                    self.rt_buf = [b' '; 64];
                    self.rt_mask = 0;
                    self.rt_known = 0;
                    self.rt_pending = [None; 16];
                }
                let new: Option<Vec<u8>> = if !ver_b {
                    c.zip(d).map(|(c, d)| bytes4(c, d).to_vec())
                } else {
                    d.map(|d| vec![(d >> 8) as u8, d as u8])
                };
                let known = self.rt_known & (1 << seg) != 0 || g.corrected[3] || (!ver_b && g.corrected[2]);
                let i = seg * seg_len;
                let accepted = new.is_some_and(|new| {
                    confirm(&mut self.rt_buf[i..i + seg_len], known, &mut self.rt_pending[seg], &new)
                });
                if accepted {
                    self.rt_known |= 1 << seg;
                    self.rt_mask |= 1 << seg;
                    if let Some(t) = self.rt_complete() {
                        if self.rt.as_ref() != Some(&t) {
                            ev.push(format!("RT \"{}\"", text::decode(&t).0));
                            if self.rt_history.back() != Some(&t) {
                                self.rt_history.push_back(t.clone());
                                if self.rt_history.len() > 30 {
                                    self.rt_history.pop_front();
                                }
                            }
                            self.rt = Some(t);
                        }
                    }
                }
            }
            (3, false) => {
                if let (Some(c), Some(d)) = (c, d) {
                    let app = (b & 0x1F) as u8;
                    let label = if app == 0 { "-".to_string() } else { group_label(app >> 1, app & 1 == 1) };
                    if self.oda.insert(d, label.clone()).is_none() {
                        ev.push(format!("ODA {:04X} {} on {}", d, oda_name(d), label));
                    }
                    if app != 0 {
                        match d {
                            AID_ERT if app & 1 == 0 => {
                                self.ert_group = Some(app >> 1);
                                self.ert_utf8 = c & 1 == 1;
                            }
                            AID_RTPLUS => self.rtplus_group = Some((app >> 1, app & 1 == 1)),
                            _ => {}
                        }
                    }
                    if let Some(pi) = g.blocks[0].filter(|_| !g.corrected.iter().any(|&corrected| corrected)) {
                        if AID_TMC.contains(&d) {
                            if self.tmc_group.is_some_and(|(old_pi, _)| old_pi != pi) {
                                self.tmc = Tmc::default();
                                self.tmc_group = None;
                            }
                            if app != 0 {
                                self.tmc_group = (app & 1 == 0).then_some((pi, app >> 1));
                            }
                            self.tmc.on_3a_message(c);
                        } else if app != 0 && app & 1 == 0 && self.tmc_group == Some((pi, app >> 1)) {
                            self.tmc_group = None;
                        }
                    }
                }
            }
            (4, false) => {
                if let (Some(c), Some(d)) = (c, d) {
                    if let Some(s) = decode_ct(b, c, d) {
                        self.ct = Some(s);
                    }
                }
            }
            (10, false) => {
                if let (Some(c), Some(d)) = (c, d) {
                    let seg = (b & 1) as usize;
                    self.ptyn_buf[seg * 4..seg * 4 + 4].copy_from_slice(&bytes4(c, d));
                    self.ptyn_mask |= 1 << seg;
                    if self.ptyn_mask == 3 {
                        self.ptyn = Some(self.ptyn_buf.to_vec());
                    }
                }
            }
            (14, _) => {
                if let Some(d) = d {
                    self.apply_eon(b, c, d, ver_b, &mut ev);
                }
            }
            (15, false) => {
                if let (Some(c), Some(d)) = (c, d) {
                    let seg = (b & 7) as usize;
                    self.long_ps_buf[seg * 4..seg * 4 + 4].copy_from_slice(&bytes4(c, d));
                    self.long_ps_mask |= 1 << seg;
                    self.update_long_ps();
                }
            }
            _ => {}
        }

        // ODA groups announced in 3A (never on the fixed-meaning groups handled above)
        let oda_type = !ver_b && !matches!(gtype, 0..=4 | 10 | 14 | 15);
        if let (true, Some(c), Some(d)) = (oda_type, c, d) {
            if self.ert_group == Some(gtype) {
                let seg = (b & 0x1F) as usize;
                self.ert_buf[seg * 4..seg * 4 + 4].copy_from_slice(&bytes4(c, d));
                self.ert_mask |= 1 << seg;
                self.update_ert();
            }
            if self.rtplus_group == Some((gtype, false)) {
                self.apply_rtplus(b, c, d, &mut ev);
            }
            if !g.corrected[0] && g.blocks[0].is_some_and(|pi| self.tmc_group == Some((pi, gtype))) {
                if let Some(e) = self.tmc.on_group(b, c, d) {
                    ev.push(e);
                }
            }
        }
        ev
    }

    pub fn di_text(&self) -> String {
        const NAMES: [(u8, &str, &str); 4] =
            [(0, "stereo", "mono"), (1, "artificial head", "-"), (2, "compressed", "-"), (3, "dynamic PTY", "static PTY")];
        NAMES
            .iter()
            .filter(|(bit, _, _)| self.di_mask & (1 << bit) != 0)
            .filter_map(|&(bit, on, off)| {
                let s = if self.di & (1 << bit) != 0 { on } else { off };
                (s != "-").then_some(s)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn to_json(&self) -> Value {
        let dec = |v: &Option<Vec<u8>>| {
            v.as_ref().map(|b| {
                let (t, cs) = text::decode(b);
                json!({ "text": t, "charset": cs, "hex": text::hex(b) })
            })
        };
        json!({
            "freq_mhz": self.freq_khz as f64 / 1000.0,
            "pi": self.pi.map(|p| format!("{p:04X}")),
            "pi_info": self.pi.map(pi_info),
            "ps": dec(&self.ps),
            "ps_partial": text::decode(&self.ps_partial()).0,
            "ps_history": self.ps_history.iter().map(|v| text::decode(v).0).collect::<Vec<_>>(),
            "long_ps": self.long_ps,
            "rt": dec(&self.rt),
            "rt_partial": text::decode(&self.rt_partial()).0,
            "rt_history": self.rt_history.iter().map(|v| text::decode(v).0).collect::<Vec<_>>(),
            "rtplus": self.rtplus.tags.iter().map(|(t, s)| json!({"type": rtplus_name(*t), "text": s})).collect::<Vec<_>>(),
            "ptyn": dec(&self.ptyn),
            "ert": self.ert,
            "pty": self.pty,
            "pty_name": self.pty.map(|p| PTY_NAMES[p as usize]),
            "tp": self.tp,
            "ta": self.ta,
            "music": self.music,
            "di_stereo": (self.di_mask & 1 != 0).then_some(self.di & 1 != 0),
            "di": self.di_text(),
            "af": self.af.iter().map(|&a| a as f64 / 10.0).collect::<Vec<_>>(),
            "ecc": self.ecc.map(|e| format!("{e:02X}")),
            "lang": self.lang,
            "pin": self.pin.map(|(d, h, m)| format!("day {d} {h:02}:{m:02}")),
            "ct": self.ct,
            "oda": self.oda.iter().map(|(aid, g)| json!({
                "aid": format!("{aid:04X}"), "group": g, "name": oda_name(*aid)
            })).collect::<Vec<_>>(),
            "eon": self.eon.iter().map(|(pi, e)| json!({
                "pi": format!("{pi:04X}"), "ps": e.ps(), "tp": e.tp, "ta": e.ta, "pty": e.pty,
                "af": e.af.iter().map(|&a| a as f64 / 10.0).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "tmc": self.tmc.to_json(),
            "group_counts": self.group_counts,
            "level_db": self.level_db,
            "pilot_db": self.pilot_db,
            "rds_snr_db": self.rds_snr_db,
            "quality": self.quality,
            "blocks_ok": self.blocks_ok,
            "blocks_total": self.blocks_total,
            "blocks_corrected": self.blocks_corrected,
            "groups_total": self.groups_total,
            "captures": self.captures,
            "age_s": self.last_seen.elapsed().as_secs_f32(),
            "rds_age_s": self.last_rds.map(|t| t.elapsed().as_secs_f32()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(b: u16, c: u16, d: u16) -> Group {
        Group { blocks: [Some(0x2A93), Some(b), Some(c), Some(d)], ..Default::default() }
    }

    #[test]
    fn tmc_requires_a_matching_oda_and_clear_location_table() {
        let mut st = Station::new(102_500);
        let user = group(0x8008, 101, 12345);
        for app in [None, Some(17), Some(16)] {
            if let Some(code) = app {
                st.apply_group(&group(0x3000 | code, 0, 0xCD46));
            }
            for _ in 0..2 {
                st.apply_group(&user);
            }
            assert!(st.tmc.messages.is_empty());
        }
        let announce = group(0x3010, 17 << 6, 0xCD46);
        st.apply_group(&announce);
        let mut other_pi = user.clone();
        other_pi.blocks[0] = Some(0x2C64);
        st.apply_group(&other_pi);
        st.apply_group(&other_pi);
        assert!(st.tmc.messages.is_empty());
        assert!(st.apply_group(&user).is_empty());
        assert!(st.apply_group(&user)[0].starts_with("TMC event 101"));
        assert_eq!(st.to_json()["tmc"]["messages"][0]["location"], 12345);
    }

    #[test]
    fn corrected_block_needs_confirmation_before_display() {
        let mut st = Station::new(106_500);
        let mut g = group((2 << 12) | 0, 0x4D55, 0x5351); // "MUSQ" from a corrected block
        g.corrected[3] = true;
        st.apply_group(&g);
        assert_eq!(st.rt_display().0, ""); // not shown yet
        st.apply_group(&group((2 << 12) | 0, 0x4D55, 0x5349)); // clean "MUSI"
        st.apply_group(&group((2 << 12) | 0, 0x4D55, 0x5349));
        assert!(st.rt_display().0.starts_with("MUSI"), "{:?}", st.rt_display());
    }

    fn rt_group(seg: u16, s: &[u8; 4]) -> Group {
        let c = ((s[0] as u16) << 8) | s[1] as u16;
        let d = ((s[2] as u16) << 8) | s[3] as u16;
        group((2 << 12) | seg, c, d)
    }

    #[test]
    fn single_bad_segment_does_not_overwrite_established_text() {
        let mut st = Station::new(97_000);
        st.apply_group(&rt_group(0, b"97.0"));
        st.apply_group(&rt_group(1, b" MHz"));
        st.apply_group(&rt_group(2, b"\r   "));
        assert_eq!(st.rt.as_deref(), Some(&b"97.0 MHz"[..]));
        st.apply_group(&rt_group(1, b" LMz")); // one corrupted block
        st.apply_group(&rt_group(1, b" MHz"));
        assert_eq!(st.rt.as_deref(), Some(&b"97.0 MHz"[..]));
        st.apply_group(&rt_group(1, b" kHz")); // a genuine change, seen twice
        st.apply_group(&rt_group(1, b" kHz"));
        assert_eq!(st.rt.as_deref(), Some(&b"97.0 kHz"[..]));
    }

    #[test]
    fn partial_ps_keeps_known_characters() {
        let mut st = Station::new(88_000);
        let ps = |seg: u16, s: &[u8; 2]| group(seg, 0, ((s[0] as u16) << 8) | s[1] as u16);
        st.apply_group(&ps(0, b"NB"));
        st.apply_group(&ps(1, b"T "));
        st.apply_group(&ps(0, b"NB")); // new cycle starts before 2 and 3 arrive
        st.apply_group(&ps(3, b".0"));
        assert_eq!(st.ps_display(), ("NBT __.0".to_string(), false));
    }

    #[test]
    fn rtplus_title_and_artist() {
        let mut st = Station::new(95_000);
        // RT "Song Name - Artist X"
        let rt = b"Song Name - Artist X\r   ";
        for seg in 0..6u16 {
            let s: [u8; 4] = rt[seg as usize * 4..seg as usize * 4 + 4].try_into().unwrap();
            st.apply_group(&rt_group(seg, &s));
        }
        assert_eq!(st.rt.as_deref(), Some(&b"Song Name - Artist X"[..]));
        // 3A: RT+ (AID 4BD7) carried in group 11A (app group code 11<<1 | 0)
        st.apply_group(&group((3 << 12) | (11 << 1), 0, AID_RTPLUS));
        // tag1: ITEM.TITLE (1) start 0 len 9-1=8 ; tag2: ITEM.ARTIST (4) start 12 len 8-1=7
        let (t1, s1, l1, t2, s2, l2) = (1u16, 0u16, 8u16, 4u16, 12u16, 7u16);
        let b = (11 << 12) | (1 << 3) | (t1 >> 3);
        let c = ((t1 & 7) << 13) | (s1 << 7) | (l1 << 1) | (t2 >> 5);
        let d = ((t2 & 0x1F) << 11) | (s2 << 5) | l2;
        st.apply_group(&group(b, c, d));
        assert_eq!(st.rtplus.tags.get(&1).map(String::as_str), Some("Song Name"));
        assert_eq!(st.rtplus.tags.get(&4).map(String::as_str), Some("Artist X"));
    }

    #[test]
    fn eon_and_traffic_flags() {
        let mut st = Station::new(97_000);
        // 0A with TP=1, TA=1
        let ev = st.apply_group(&group((1 << 10) | (1 << 4), 0, 0x4142));
        assert!(st.ta_active());
        assert!(ev.iter().any(|e| e.contains("Traffic announcement started")));
        // EON PS of other network 2C64 = "RTAF06  "
        for (v, s) in [(0u16, b"RT"), (1, b"AF"), (2, b"06"), (3, b"  ")] {
            st.apply_group(&group((14 << 12) | v, ((s[0] as u16) << 8) | s[1] as u16, 0x2C64));
        }
        // 14B TA(ON)=1, TP(ON)=1
        let ev = st.apply_group(&Group {
            blocks: [Some(0x2A93), Some((14 << 12) | (1 << 11) | (1 << 4) | (1 << 3)), Some(0x2A93), Some(0x2C64)],
            ..Default::default()
        });
        let e = &st.eon[&0x2C64];
        assert_eq!(e.ps().as_deref(), Some("RTAF06  "));
        assert_eq!((e.tp, e.ta), (Some(true), Some(true)));
        assert!(ev.iter().any(|x| x.contains("EON traffic")));
    }

    #[test]
    fn mjd_example_from_standard() {
        // IEC 62106 example: MJD 45218 = 1982-09-06
        assert_eq!(mjd_to_ymd(45218), (1982, 9, 6));
        assert_eq!(mjd_to_ymd(61367), (2026, 11, 23));
    }
}
