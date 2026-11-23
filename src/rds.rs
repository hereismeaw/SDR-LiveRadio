//! RDS block layer (IEC 62106): checkwords, offset words, block/group synchronisation.

/// Generator polynomial x^10 + x^8 + x^7 + x^5 + x^4 + x^3 + 1.
pub const POLY: u32 = 0x5B9;
pub const OFF_A: u16 = 0x0FC;
pub const OFF_B: u16 = 0x198;
pub const OFF_C: u16 = 0x168;
pub const OFF_CP: u16 = 0x350; // C' (version B groups)
pub const OFF_D: u16 = 0x1B4;

/// Remainder of a 26-bit word modulo POLY. For a valid block this equals its offset word.
pub fn rem10(w: u32) -> u16 {
    let mut r = w;
    for i in (10..26).rev() {
        if r & (1 << i) != 0 {
            r ^= POLY << (i - 10);
        }
    }
    (r & 0x3FF) as u16
}

/// Build a 26-bit block: 16 data bits followed by checkword XOR offset.
pub fn encode_block(data: u16, offset: u16) -> u32 {
    let w = (data as u32) << 10;
    w | (rem10(w) ^ offset) as u32
}

/// Syndrome -> error pattern for every burst of length 1..=5 inside a 26-bit block.
/// The RDS (26,16) shortened cyclic code is designed to correct such bursts; any
/// syndrome shared by two different patterns is left out (ambiguous).
fn burst_table() -> &'static std::collections::HashMap<u16, u32> {
    static TABLE: std::sync::OnceLock<std::collections::HashMap<u16, u32>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut map: std::collections::HashMap<u16, Option<u32>> = std::collections::HashMap::new();
        for len in 1..=5u32 {
            // patterns of `len` bits with both end bits set
            let inner = if len <= 2 { 1 } else { 1u32 << (len - 2) };
            for mid in 0..inner {
                let pat = if len == 1 { 1 } else { (1 << (len - 1)) | (mid << 1) | 1 };
                for shift in 0..=(26 - len) {
                    let e = pat << shift;
                    let s = rem10(e);
                    match map.get(&s) {
                        None => {
                            map.insert(s, Some(e));
                        }
                        Some(Some(prev)) if *prev != e => {
                            map.insert(s, None);
                        }
                        _ => {}
                    }
                }
            }
        }
        map.into_iter().filter_map(|(s, e)| e.map(|e| (s, e))).collect()
    })
}

/// Error pattern for syndrome `s`; `max_bits` limits how many bits may be flipped.
fn correctable(s: u16, max_bits: u32) -> Option<u32> {
    burst_table().get(&s).copied().filter(|e| e.count_ones() <= max_bits)
}

#[derive(Clone, Debug, Default)]
pub struct Group {
    /// Blocks A..D; `None` when the checkword failed.
    pub blocks: [Option<u16>; 4],
    /// Blocks recovered by error correction (less trustworthy than a clean checkword).
    pub corrected: [bool; 4],
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BlockStats {
    pub ok: u32,
    pub total: u32,
    pub corrected: u32,
}

pub fn group_label(gtype: u8, version_b: bool) -> String {
    format!("{}{}", gtype, if version_b { 'B' } else { 'A' })
}

/// Find group alignment in a bit stream and return all groups with a valid block B.
pub fn find_groups(bits: &[u8]) -> (Vec<Group>, BlockStats) {
    let n = bits.len();
    let mut stats = BlockStats::default();
    let mut groups = Vec::new();
    if n < 104 {
        return (groups, stats);
    }
    let word_at = |i: usize| -> u32 { bits[i..i + 26].iter().fold(0u32, |w, &b| (w << 1) | b as u32) };

    // Vote for the group phase (mod 104) using every bit position that matches an offset word.
    let mut votes = [0u32; 104];
    for i in 0..=n - 26 {
        let idx: i64 = match rem10(word_at(i)) {
            OFF_A => 0,
            OFF_B => 1,
            OFF_C | OFF_CP => 2,
            OFF_D => 3,
            _ => continue,
        };
        votes[(i as i64 - 26 * idx).rem_euclid(104) as usize] += 1;
    }
    let (phase, &best) = votes.iter().enumerate().max_by_key(|(_, v)| **v).unwrap();
    // Noise alone produces ~13 random offset matches per 2.3 s spread over 104 phases;
    // a real RDS signal gives ~100 on one phase.
    if best < 6 {
        stats.total = (n / 26) as u32;
        return (groups, stats);
    }

    let expected: [&[u16]; 4] = [&[OFF_A], &[OFF_B], &[OFF_C, OFF_CP], &[OFF_D]];
    let mut g = phase;
    while g + 104 <= n {
        let mut blocks = [None; 4];
        let mut words = [0u32; 4];
        let mut raw_ok = 0;
        for j in 0..4 {
            let w = word_at(g + 26 * j);
            words[j] = w;
            if expected[j].contains(&rem10(w)) {
                blocks[j] = Some((w >> 10) as u16);
                raw_ok += 1;
            }
        }
        // Error correction only when the group is otherwise anchored. The code can fix
        // bursts up to 5 bits, but those patterns cover 367 of 1024 syndromes, so a badly
        // damaged block would be "corrected" into wrong data ~36% of the time. Limit to
        // at most 2 flipped bits (120 syndromes, ~12%) with 3 clean blocks, 1 bit with 2.
        let mut corrected = [false; 4];
        if raw_ok >= 2 {
            let max_bits = if raw_ok >= 3 { 2 } else { 1 };
            for j in 0..4 {
                if blocks[j].is_some() {
                    continue;
                }
                let s = rem10(words[j]);
                for &off in expected[j] {
                    if let Some(e) = correctable(s ^ off, max_bits) {
                        blocks[j] = Some(((words[j] ^ e) >> 10) as u16);
                        corrected[j] = true;
                        stats.corrected += 1;
                        break;
                    }
                }
            }
        }
        stats.total += 4;
        stats.ok += raw_ok;
        if blocks[1].is_some() {
            groups.push(Group { blocks, corrected });
        }
        g += 104;
    }
    (groups, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_block_syndrome_is_offset() {
        for &off in &[OFF_A, OFF_B, OFF_C, OFF_CP, OFF_D] {
            for data in [0u16, 0x2A01, 0xFFFF, 0x1234] {
                assert_eq!(rem10(encode_block(data, off)), off);
            }
        }
    }

    #[test]
    fn group_sync_and_single_bit_correction() {
        let groups = [[0x2A01u16, 0x0408, 0xE150, 0x424B], [0x2A01, 0x2400, 0x5448, 0x4149]];
        let mut bits = vec![1u8, 0, 1]; // misalignment
        for _ in 0..3 {
            for g in &groups {
                for (j, &d) in g.iter().enumerate() {
                    let w = encode_block(d, [OFF_A, OFF_B, OFF_C, OFF_D][j]);
                    bits.extend((0..26).rev().map(|k| ((w >> k) & 1) as u8));
                }
            }
        }
        bits[3 + 104 + 60] ^= 1; // corrupt block C of the 2nd group
        let (found, stats) = find_groups(&bits);
        assert_eq!(found.len(), 6);
        assert_eq!(stats.corrected, 1);
        assert_eq!(found[1].blocks[2], Some(0x5448));
    }

    #[test]
    fn every_burst_up_to_5_bits_is_corrected() {
        let data = 0x5448u16;
        let good = encode_block(data, OFF_C);
        let mut n = 0;
        for len in 1..=5u32 {
            let inner = if len <= 2 { 1 } else { 1u32 << (len - 2) };
            for mid in 0..inner {
                let pat = if len == 1 { 1 } else { (1 << (len - 1)) | (mid << 1) | 1 };
                for shift in 0..=(26 - len) {
                    let bad = good ^ (pat << shift);
                    let e = correctable(rem10(bad) ^ OFF_C, 5).expect("burst not correctable");
                    assert_eq!(((bad ^ e) >> 10) as u16, data);
                    n += 1;
                }
            }
        }
        assert!(n > 300);
    }
}
