//! RDS text decoding: EBU Latin (IEC 62106 Annex E) or Thai TIS-620 / Windows-874.
//!
//! Standard RDS has no Thai characters; Thai stations that send Thai text in PS/RT
//! put TIS-620 bytes there. We pick TIS-620 automatically when the high bytes look Thai.

const EBU_HIGH: [&str; 8] = [
    "áàéèíìóòúùÑÇŞβ¡Ĳ", // 0x80
    "âäêëîïôöûüñçşğıĳ", // 0x90
    "ªα©‰Ğěňőπ€£$←↑→↓", // 0xA0
    "º¹²³±İńűµ¿÷°¼½¾§", // 0xB0
    "ÁÀÉÈÍÌÓÒÚÙŘČŠŽÐĿ", // 0xC0
    "ÂÄÊËÎÏÔÖÛÜřčšžđŀ", // 0xD0
    "ÃÅÆŒŷÝÕØÞŊŔĆŚŹŦð", // 0xE0
    "ãåæœŵýõøþŋŕćśźŧ ", // 0xF0
];

fn ebu(b: u8) -> char {
    match b {
        0x20..=0x7E => b as char,
        0x80..=0xFF => EBU_HIGH[((b - 0x80) >> 4) as usize]
            .chars()
            .nth((b & 0x0F) as usize)
            .unwrap_or('?'),
        _ => ' ',
    }
}

/// Decode bytes; returns (text, charset name).
pub fn decode(bytes: &[u8]) -> (String, &'static str) {
    let thai = bytes.iter().filter(|&&b| (0xA1..=0xFB).contains(&b)).count();
    let high = bytes.iter().filter(|&&b| b >= 0x80).count();
    if thai > 0 && thai * 2 >= high {
        let (s, _, _) = encoding_rs::WINDOWS_874.decode(bytes);
        let s = s.chars().map(|c| if (c as u32) < 0x20 { ' ' } else { c }).collect();
        (s, "TIS-620")
    } else {
        (bytes.iter().map(|&b| ebu(b)).collect(), "EBU")
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_have_16_chars() {
        for r in EBU_HIGH {
            assert_eq!(r.chars().count(), 16, "{r}");
        }
    }

    #[test]
    fn thai_round_trip() {
        let (b, _, _) = encoding_rs::WINDOWS_874.encode("สวัสดี FM");
        assert_eq!(decode(&b), ("สวัสดี FM".to_string(), "TIS-620"));
        assert_eq!(decode(b"BKK RDS1").1, "EBU");
    }
}
