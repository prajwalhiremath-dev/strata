//! Variable-length integers (varints), used by the SSTable block format.
//!
//! Unsigned LEB128: each byte carries 7 bits of the number, least significant
//! group first, and its high bit says "more bytes follow". Small numbers -
//! which most key and value lengths are - take one byte instead of four.
//!
//! ```text
//! 300 = 0b1_0010_1100  ->  [1010_1100] [0000_0010]
//!                           ^ more      ^ last
//! ```
//!
//! Decoders treat their input as untrusted: truncated, overlong (more bytes
//! than the type can need) and overflowing encodings are `Corruption`.

use crate::{Error, Result};

pub(crate) fn put_varint32(dst: &mut Vec<u8>, v: u32) {
    put_varint64(dst, u64::from(v));
}

pub(crate) fn put_varint64(dst: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        dst.push((v as u8) | 0x80);
        v >>= 7;
    }
    dst.push(v as u8);
}

pub(crate) fn get_varint32(src: &[u8]) -> Result<(u32, usize)> {
    let (v, n) = get_varint(src, 32)?;
    Ok((v as u32, n))
}

pub(crate) fn get_varint64(src: &[u8]) -> Result<(u64, usize)> {
    get_varint(src, 64)
}

/// Decode a varint of at most `bits` bits from the front of `src`.
fn get_varint(src: &[u8], bits: u32) -> Result<(u64, usize)> {
    let max_bytes = bits.div_ceil(7) as usize; // 5 for u32, 10 for u64
    let mut v: u64 = 0;
    for (i, &b) in src.iter().enumerate().take(max_bytes) {
        let shift = 7 * i as u32;
        let group = u64::from(b & 0x7f);
        // The last allowed byte may only use the bits the type has left.
        let room = bits - shift; // bits still available, >= 1
        if room < 7 && group >> room != 0 {
            return Err(Error::Corruption(format!("varint overflows u{bits}")));
        }
        v |= group << shift;
        if b & 0x80 == 0 {
            return Ok((v, i + 1));
        }
    }
    if src.len() >= max_bytes {
        Err(Error::Corruption(format!(
            "varint longer than u{bits} allows"
        )))
    } else {
        Err(Error::Corruption("truncated varint".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn varint_round_trips() {
        for (v, len) in [
            (0u32, 1),
            (1, 1),
            (127, 1),
            (128, 2),
            (16383, 2),
            (16384, 3),
            (u32::MAX, 5),
        ] {
            let mut buf = Vec::new();
            put_varint32(&mut buf, v);
            assert_eq!(buf.len(), len, "length of {v}");
            assert_eq!(get_varint32(&buf).unwrap(), (v, len));
            buf.extend_from_slice(b"tail");
            assert_eq!(
                get_varint32(&buf).unwrap(),
                (v, len),
                "ignores what follows"
            );
        }
        for (v, len) in [(0u64, 1), (128, 2), (u32::MAX as u64, 5), (u64::MAX, 10)] {
            let mut buf = Vec::new();
            put_varint64(&mut buf, v);
            assert_eq!(buf.len(), len, "length of {v}");
            assert_eq!(get_varint64(&buf).unwrap(), (v, len));
        }
    }

    #[test]
    fn varint_rejects_bad_input() {
        let corrupt32 = |b: &[u8]| matches!(get_varint32(b), Err(Error::Corruption(_)));
        let corrupt64 = |b: &[u8]| matches!(get_varint64(b), Err(Error::Corruption(_)));
        assert!(corrupt32(&[]), "empty");
        assert!(corrupt32(&[0x80]), "truncated");
        assert!(corrupt32(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]), "overlong");
        assert!(corrupt32(&[0xff, 0xff, 0xff, 0xff, 0x1f]), "overflows u32");
        assert!(corrupt64(&[]), "empty");
        assert!(corrupt64(&[0x80; 11]), "overlong");
        assert!(
            corrupt64(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02]),
            "overflows u64"
        );
    }

    proptest! {
        #[test]
        fn varint_decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..16)) {
            let _ = get_varint32(&bytes);
            let _ = get_varint64(&bytes);
        }
    }
}
