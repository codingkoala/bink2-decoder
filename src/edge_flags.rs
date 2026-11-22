//! Row/column edge flag lists (SPEC §3.3). A flag value of 1 switches one edge of the loop
//! filter off (SPEC §4.10).

use crate::bits::BitReader;
use crate::error::Result;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    None,
    Literal,
    Toggle,
    Explicit,
}

/// Reads one edge flag list and returns the `count` flags `B[0] … B[count-1]`. Both the raw and
/// the coded form are written from position 0 (SPEC draft 6).
pub fn read_edge_flags(br: &mut BitReader, count: usize) -> Result<Vec<bool>> {
    let mut f = vec![false; count];
    if !br.bit()? {
        // raw form
        for v in f.iter_mut() {
            *v = br.bit()?;
        }
        return Ok(f);
    }
    let mut pos = 0usize;
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
                // An `extra` larger than what is left is cut to `left` (SPEC §3.3); the list
                // then ends.
                let extra = (br.bits(w as u32)? as usize).min(left);
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
        assert_eq!(f, flags("10110"));
        assert_eq!(used, 6);
    }

    #[test]
    fn literal_groups() {
        // coded, literal (flag=1, 4 more bits 0110), literal (flag=0, 1 more bit: 1)
        let (f, used) = decode(&["1", "0", "1", "0110", "0", "0", "1"], 7);
        // B[0..5] = 1 0110, B[5..7] = 0 1
        assert_eq!(f, flags("1011001"));
        assert_eq!(used, 1 + 1 + 1 + 4 + 1 + 1 + 1);
    }

    #[test]
    fn run_covering_everything() {
        // count 3: run, w = 2, flag 1, run = min(4, 3) = 3, left 0 -> no extra
        let (f, used) = decode(&["1", "1", "1"], 3);
        assert_eq!(f, flags("111"));
        assert_eq!(used, 3);
    }

    #[test]
    fn run_then_toggle() {
        // count 10: run, w = 4 (left 10), flag 1, run 6, left 4, extra = bits(4) = 1 (< 15)
        // -> run 7, left 3, TOGGLE. Next: run, w = 2 (left 3), toggle: flag 0, run = 3, left 0.
        let (f, used) = decode(&["1", "1", "1", "1000", "1"], 10);
        assert_eq!(f, flags("1111111000"));
        assert_eq!(used, 1 + 1 + 1 + 4 + 1);
    }

    #[test]
    fn toggle_then_literal() {
        // count 9: run w=4, flag 0, run 6, left 3, extra bits(4)=0 -> TOGGLE, left 3.
        // literal: flag = !0 = 1 (no bit read), n = 3, two more bits "01".
        let (f, used) = decode(&["1", "1", "0", "0000", "0", "01"], 9);
        assert_eq!(f, flags("000000101"));
        assert_eq!(used, 1 + 1 + 1 + 4 + 1 + 2);
    }

    #[test]
    fn explicit_mode() {
        // count 40: run w=5, flag 1, run 7, left 33, extra 31 -> run 38, left 2, EXPLICIT.
        // Next: run w=2 (left 2), explicit: flag = bit() = 0, run = min(4, 2) = 2.
        let (f, used) = decode(&["1", "1", "1", "11111", "1", "0"], 40);
        let mut want = Vec::new();
        want.extend(std::iter::repeat_n(true, 38));
        want.extend([false, false]);
        assert_eq!(f, want);
        assert_eq!(used, 1 + 1 + 1 + 5 + 1 + 1);
    }

    #[test]
    fn overlong_run_is_clipped() {
        // count 8: run w=4, flag 1, run 6, left 2, extra 15 is cut to 2: the run covers
        // B[0..8] and no further bits are read (the trailing "0101" stays unread).
        let (f, used) = decode(&["1", "1", "1", "1111", "0101"], 8);
        assert_eq!(f, flags("11111111"));
        assert_eq!(used, 1 + 1 + 1 + 4);
        // contrast: a run clipped by min(run, left) reads no extra at all. count 7, literal
        // group of 5 (left 2), then run w=2, flag 0, run min(4, 2) = 2 -> left 0.
        let (f, used) = decode(&["1", "0", "1", "1111", "1", "0"], 7);
        assert_eq!(f, flags("1111100"));
        assert_eq!(used, 1 + 1 + 1 + 4 + 1 + 1);
    }

    #[test]
    fn raw_and_coded_forms_share_the_layout() {
        // SPEC draft 6: both forms start at B[0]. The same flags 10110 as a raw list and as one
        // coded literal group give the same result and the same length.
        let (raw, _) = decode(&["0", "10110"], 5);
        let (coded, used) = decode(&["1", "0", "1", "0110"], 5);
        assert_eq!(raw, coded);
        assert_eq!(coded.len(), 5);
        assert_eq!(used, 7);
        // a coded run starting at position 0: count 4, w = 4, flag 1, run min(6, 4) = 4
        let (f, _) = decode(&["1", "1", "1"], 4);
        assert_eq!(f, flags("1111"));
    }
}
