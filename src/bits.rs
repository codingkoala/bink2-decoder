//! LSB-first bit reader (SPEC §1.1).

use crate::error::{Error, Result};

/// Reads bits LSB-first: bit 0 of byte 0 comes first.
///
/// Reading past the end of the data is an error ([`Error::Truncated`]).
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits.
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// Creates a reader positioned at the first bit of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    /// Current position in bits.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Total number of bits in the data.
    pub fn len_bits(&self) -> usize {
        self.data.len() * 8
    }

    /// Bits left to read.
    pub fn remaining(&self) -> usize {
        self.len_bits().saturating_sub(self.pos)
    }

    /// Moves to an absolute byte offset.
    pub fn seek_byte(&mut self, byte: usize) -> Result<()> {
        if byte > self.data.len() {
            return Err(Error::Truncated);
        }
        self.pos = byte * 8;
        Ok(())
    }

    /// Returns up to 32 bits starting at the current position without consuming them.
    /// Bits past the end of the data read as 0.
    pub fn peek(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let byte = self.pos / 8;
        let shift = (self.pos % 8) as u32;
        let mut acc: u64 = 0;
        for i in 0..5 {
            let b = self.data.get(byte + i).copied().unwrap_or(0) as u64;
            acc |= b << (8 * i);
        }
        let v = acc >> shift;
        (v & ((1u64 << n) - 1)) as u32
    }

    /// Consumes `n` bits.
    pub fn skip(&mut self, n: usize) -> Result<()> {
        if n > self.remaining() {
            return Err(Error::Truncated);
        }
        self.pos += n;
        Ok(())
    }

    /// `bits(n)`: reads `n` bits (0 ..= 32). The first bit read is bit 0 of the result.
    pub fn bits(&mut self, n: u32) -> Result<u32> {
        if n > 32 {
            return Err(Error::InvalidData("bit count over 32"));
        }
        let v = self.peek(n);
        self.skip(n as usize)?;
        Ok(v)
    }

    /// `bit()`: reads one bit.
    pub fn bit(&mut self) -> Result<bool> {
        Ok(self.bits(1)? != 0)
    }

    /// `ones(max)`: counts 1-bits until a 0-bit (consumed). Stops at `max` without a terminator.
    pub fn ones(&mut self, max: u32) -> Result<u32> {
        let mut n = 0;
        while n < max {
            if !self.bit()? {
                break;
            }
            n += 1;
        }
        Ok(n)
    }

    /// `zeros(max)`: counts 0-bits until a 1-bit (consumed). Stops at `max` without a terminator.
    pub fn zeros(&mut self, max: u32) -> Result<u32> {
        let mut n = 0;
        while n < max {
            if self.bit()? {
                break;
            }
            n += 1;
        }
        Ok(n)
    }

    /// `align()`: skips to the next byte boundary (stays put if already aligned).
    pub fn align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsb_first_order() {
        // 0b1010_0110: bits in read order are 0,1,1,0,0,1,0,1
        let d = [0b1010_0110u8, 0xFF];
        let mut r = BitReader::new(&d);
        let seq: Vec<bool> = (0..8).map(|_| r.bit().unwrap()).collect();
        assert_eq!(seq, [false, true, true, false, false, true, false, true]);
        let mut r = BitReader::new(&d);
        assert_eq!(r.bits(3).unwrap(), 0b110);
        assert_eq!(r.bits(0).unwrap(), 0);
        // next 7 bits: byte0 bits 3..7 = 0b10100, then byte1 bits 0..1 = 0b11
        assert_eq!(r.bits(7).unwrap(), 0b11_10100);
        assert_eq!(r.position(), 10);
    }

    #[test]
    fn bits_32_and_truncation() {
        let d = [0x78, 0x56, 0x34, 0x12, 0xAB];
        let mut r = BitReader::new(&d);
        r.bits(4).unwrap();
        assert_eq!(r.bits(32).unwrap(), 0xB123_4567);
        assert_eq!(r.bits(4).unwrap(), 0xA);
        assert_eq!(r.bits(1), Err(Error::Truncated));
        assert!(r.bits(33).is_err());
    }

    #[test]
    fn ones_zeros_limits() {
        // read order: 1 1 1 0 | 1 1 1 1 ...
        let d = [0b1111_0111u8, 0xFF];
        let mut r = BitReader::new(&d);
        assert_eq!(r.ones(12).unwrap(), 3);
        assert_eq!(r.position(), 4); // terminator consumed
        assert_eq!(r.ones(3).unwrap(), 3);
        assert_eq!(r.position(), 7); // no terminator at max
        let d = [0b0001_0000u8, 0x00];
        let mut r = BitReader::new(&d);
        assert_eq!(r.zeros(12).unwrap(), 4);
        assert_eq!(r.position(), 5);
        assert_eq!(r.zeros(3).unwrap(), 3);
        assert_eq!(r.position(), 8);
        assert_eq!(r.zeros(0).unwrap(), 0);
        // running out of data while counting is an error
        let mut r = BitReader::new(&[0u8]);
        assert_eq!(r.zeros(12), Err(Error::Truncated));
    }

    #[test]
    fn align_and_seek() {
        let d = [0u8; 4];
        let mut r = BitReader::new(&d);
        r.align();
        assert_eq!(r.position(), 0);
        r.bits(1).unwrap();
        r.align();
        assert_eq!(r.position(), 8);
        r.seek_byte(4).unwrap();
        assert_eq!(r.remaining(), 0);
        assert!(r.seek_byte(5).is_err());
    }
}
