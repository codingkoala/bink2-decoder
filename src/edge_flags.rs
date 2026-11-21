//! Row/column edge flag lists (SPEC §3.3). Parsed to stay in sync; their meaning is unknown.

use crate::bits::BitReader;
use crate::error::{Error, Result};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    None,
    Literal,
    Toggle,
    Explicit,
}

/// Reads one edge flag list and returns the `count + 1` flags `F[0] … F[count]`.
pub fn read_edge_flags(br: &mut BitReader, count: usize) -> Result<Vec<bool>> {
    let mut f = vec![false; count + 1];
    if !br.bit()? {
        // raw form
        for v in f.iter_mut().take(count) {
            *v = br.bit()?;
        }
        return Ok(f);
    }
    let mut pos = 1usize;
    let mut left = count;
    let mut mode = Mode::None;
    let mut flag = false;
    while left > 0 {
        if !br.bit()? {
            // literal group
            flag = if mode == Mode::Toggle { !flag } else { br.bit()? };
            mode = Mode::Literal;
            let n = left.min(5);
            f[pos] = flag;
            for v in &mut f[pos + 1..pos + n] {
                *v = br.bit()?;
            }
            pos += n;
            left -= n;
        } else {
            // run
            let w = if left < 4 {
                2
            } else if left < 16 {
                4
            } else {
                5
            };
            let mut run;
            if mode == Mode::Toggle {
                flag = !flag;
                run = w + 1;
            } else {
                flag = br.bit()?;
                run = w + 2;
            }
            run = run.min(left);
            left -= run;
            if left > 0 {
                let extra = br.bits(w as u32)? as usize;
                if extra > left {
                    return Err(Error::InvalidData("edge flag run too long"));
                }
                left -= extra;
                run += extra;
                mode = if extra == (1 << w) - 1 {
                    Mode::Explicit
                } else {
                    Mode::Toggle
                };
            }
            for v in &mut f[pos..pos + run] {
                *v = flag;
            }
            pos += run;
        }
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vlc::tests::pack;

    fn flags(s: &str) -> Vec<bool> {
        s.bytes().map(|b| b == b'1').collect()
    }

    fn decode(parts: &[&str], count: usize) -> (Vec<bool>, usize) {
        let d = pack(parts);
        let mut br = BitReader::new(&d);
        let f = read_edge_flags(&mut br, count).unwrap();
        (f, br.position())
    }

    #[test]
    fn raw_form() {
        let (f, used) = decode(&["0", "10110"], 5);
        assert_eq!(f, flags("101100"));
        assert_eq!(used, 6);
    }

    #[test]
    fn literal_groups() {
        // coded, literal (flag=1, 4 more bits 0110), literal (flag=0, 1 more bit: 1)
        let (f, used) = decode(&["1", "0", "1", "0110", "0", "0", "1"], 7);
        // F[0] = 0, F[1..6] = 1 0110, F[6..8] = 0 1
        assert_eq!(f, flags("01011001"));
        assert_eq!(used, 1 + 1 + 1 + 4 + 1 + 1 + 1);
    }

    #[test]
    fn run_covering_everything() {
        // count 3: run, w = 2, flag 1, run = min(4, 3) = 3, left 0 -> no extra
        let (f, used) = decode(&["1", "1", "1"], 3);
        assert_eq!(f, flags("0111"));
        assert_eq!(used, 3);
    }

    #[test]
    fn run_then_toggle() {
        // count 10: run, w = 4 (left 10), flag 1, run 6, left 4, extra = bits(4) = 1 (< 15)
        // -> run 7, left 3, TOGGLE. Next: run, w = 2 (left 3), toggle: flag 0, run = 3, left 0.
        let (f, used) = decode(&["1", "1", "1", "1000", "1"], 10);
        assert_eq!(f, flags("01111111000"));
        assert_eq!(used, 1 + 1 + 1 + 4 + 1);
    }

    #[test]
    fn toggle_then_literal() {
        // count 9: run w=4, flag 0, run 6, left 3, extra bits(4)=0 -> TOGGLE, left 3.
        // literal: flag = !0 = 1 (no bit read), n = 3, two more bits "01".
        let (f, used) = decode(&["1", "1", "0", "0000", "0", "01"], 9);
        assert_eq!(f, flags("0000000101"));
        assert_eq!(used, 1 + 1 + 1 + 4 + 1 + 2);
    }

    #[test]
    fn explicit_mode() {
        // count 40: run w=5, flag 1, run 7, left 33, extra 31 -> run 38, left 2, EXPLICIT.
        // Next: run w=2 (left 2), explicit: flag = bit() = 0, run = min(4, 2) = 2.
        let (f, used) = decode(&["1", "1", "1", "11111", "1", "0"], 40);
        let mut want = vec![false];
        want.extend(std::iter::repeat_n(true, 38));
        want.extend([false, false]);
        assert_eq!(f, want);
        assert_eq!(used, 1 + 1 + 1 + 5 + 1 + 1);
    }

    #[test]
    fn overlong_run_is_error() {
        // count 8: run w=4, flag 1, run 6, left 2, extra 15 > 2
        let d = pack(&["1", "1", "1", "1111"]);
        let mut br = BitReader::new(&d);
        assert!(read_edge_flags(&mut br, 8).is_err());
    }
}
