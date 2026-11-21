//! Variable-length code tables (TABLES.md), decoded with a lookup table.

use crate::bits::BitReader;
use crate::error::{Error, Result};
use crate::tables;
use std::sync::OnceLock;

/// A prefix-free code table, decoded by peeking `max_len` bits.
#[derive(Debug)]
pub struct Vlc {
    max_len: u32,
    /// Indexed by the next `max_len` bits (first bit read = bit 0); `(symbol, length)`, length 0 = invalid.
    lut: Vec<(u8, u8)>,
}

impl Vlc {
    /// Builds a table from bit strings in read order. Panics if the codes are not prefix-free
    /// (the inputs are crate constants, checked by tests).
    pub fn new(codes: &[&str]) -> Vlc {
        let max_len = codes.iter().map(|c| c.len() as u32).max().unwrap_or(0);
        assert!(max_len <= 16);
        let mut lut = vec![(0u8, 0u8); 1 << max_len];
        for (sym, code) in codes.iter().enumerate() {
            let (val, len) = code_value(code);
            let step = 1usize << len;
            let mut i = val as usize;
            while i < lut.len() {
                assert_eq!(lut[i].1, 0, "VLC codes are not prefix-free");
                lut[i] = (sym as u8, len as u8);
                i += step;
            }
        }
        Vlc { max_len, lut }
    }

    /// `vlc(T)`: reads one code and returns its symbol number.
    pub fn read(&self, br: &mut BitReader) -> Result<u32> {
        let idx = br.peek(self.max_len) as usize;
        let (sym, len) = self.lut[idx];
        if len == 0 {
            return Err(Error::InvalidData("invalid VLC code"));
        }
        br.skip(len as usize)?;
        Ok(sym as u32)
    }
}

/// Converts a bit string in read order into `(value, length)` for an LSB-first reader.
pub fn code_value(code: &str) -> (u32, u32) {
    let mut v = 0u32;
    for (i, ch) in code.bytes().enumerate() {
        if ch == b'1' {
            v |= 1 << i;
        }
    }
    (v, code.len() as u32)
}

macro_rules! vlc_table {
    ($fn:ident, $codes:path) => {
        #[doc = concat!("The `", stringify!($codes), "` table.")]
        pub fn $fn() -> &'static Vlc {
            static T: OnceLock<Vlc> = OnceLock::new();
            T.get_or_init(|| Vlc::new(&$codes))
        }
    };
}

vlc_table!(g_skip0, tables::G_SKIP0_CODES);
vlc_table!(g_skip1, tables::G_SKIP1_CODES);
vlc_table!(g_mv, tables::G_MV_CODES);
vlc_table!(f_dq, tables::F_DQ_CODES);
vlc_table!(f_level0, tables::F_LEVEL0_CODES);
vlc_table!(f_level1, tables::F_LEVEL1_CODES);
vlc_table!(f_skip0, tables::F_SKIP0_CODES);
vlc_table!(f_skip1, tables::F_SKIP1_CODES);

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Packs a sequence of bit strings (read order) into LSB-first bytes.
    pub fn pack(parts: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut n = 0usize;
        for p in parts {
            for ch in p.bytes() {
                if n.is_multiple_of(8) {
                    out.push(0);
                }
                if ch == b'1' {
                    *out.last_mut().unwrap() |= 1 << (n % 8);
                }
                n += 1;
            }
        }
        out
    }

    fn round_trip(codes: &[&str], vlc: &Vlc) {
        for (sym, code) in codes.iter().enumerate() {
            // the code alone, then followed by every other code
            for next in codes.iter() {
                let data = pack(&[code, next]);
                let mut br = BitReader::new(&data);
                assert_eq!(vlc.read(&mut br).unwrap(), sym as u32, "code {code}");
                assert_eq!(br.position(), code.len());
            }
            let data = pack(&[code]);
            let mut br = BitReader::new(&data);
            assert_eq!(vlc.read(&mut br).unwrap(), sym as u32);
        }
    }

    #[test]
    fn all_tables_round_trip() {
        round_trip(&tables::G_SKIP0_CODES, g_skip0());
        round_trip(&tables::G_SKIP1_CODES, g_skip1());
        round_trip(&tables::G_MV_CODES, g_mv());
        round_trip(&tables::F_DQ_CODES, f_dq());
        round_trip(&tables::F_LEVEL0_CODES, f_level0());
        round_trip(&tables::F_LEVEL1_CODES, f_level1());
        round_trip(&tables::F_SKIP0_CODES, f_skip0());
        round_trip(&tables::F_SKIP1_CODES, f_skip1());
    }

    #[test]
    fn complete_codes_have_no_invalid_entries() {
        // All tables in TABLES.md are complete (Kraft sum 1), so no bit pattern is invalid.
        for t in [
            g_skip0(),
            g_skip1(),
            g_mv(),
            f_dq(),
            f_level0(),
            f_level1(),
            f_skip0(),
            f_skip1(),
        ] {
            assert!(t.lut.iter().all(|e| e.1 != 0));
        }
    }

    #[test]
    fn truncated_code_is_error() {
        // G_MV symbol 4 is 7 bits; give only the first 3 bits in a 1-byte buffer at offset 6.
        let data = pack(&["000000", "00"]);
        let mut br = BitReader::new(&data);
        br.skip(6).unwrap();
        assert!(g_mv().read(&mut br).is_err());
    }
}
