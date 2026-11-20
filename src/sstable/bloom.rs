//! Bloom filter over a table's user keys.
//!
//! A bloom filter answers "is this key maybe in the table?" with either
//! *definitely not* or *maybe*. It is an array of `m_bits` bits; adding a key
//! sets `k` of them, chosen by hashing the key. A lookup tests the same `k`
//! bits: if any is clear the key was never added. If all are set, the key is
//! probably there, but those bits might have been set by other keys (a
//! false positive).
//!
//! ```text
//! filter block: bit array (m bytes) | k (u8)      + the usual 4-byte CRC
//! ```
//!
//! The hash and the probe sequence are part of the file format: a table
//! written today must be readable by every later build, so neither may change
//! without a new format version. Chapter 04 derives the false-positive rate.

use crate::{Error, Result};

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const MAX_PROBES: u32 = 30;
const MIN_BYTES: usize = 8;

/// FNV-1a 64 mixed by MurmurHash3's 64-bit finalizer.
///
/// FNV-1a alone is fast but its high bits mix poorly for short keys; the
/// finalizer spreads every input bit over every output bit, and double
/// hashing below uses both halves of the result.
pub(crate) fn hash64(key: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for &b in key {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

/// The number of probes `k` that minimises the false-positive rate for this
/// many bits per key: `bits_per_key × ln 2`, rounded, clamped to `1..=30`.
pub(crate) fn num_probes(bits_per_key: usize) -> u32 {
    let k = (bits_per_key as f64 * std::f64::consts::LN_2).round();
    (k as u32).clamp(1, MAX_PROBES)
}

/// The bit positions a key with hash `h` uses (Kirsch–Mitzenmacher double
/// hashing: two hash values make all `k` probes).
fn probes(h: u64, k: u32, m_bits: u64) -> impl Iterator<Item = u64> {
    let delta = (h >> 32) | 1;
    (0..u64::from(k)).map(move |i| h.wrapping_add(i.wrapping_mul(delta)) % m_bits)
}

/// Filter block contents (bit array then `k`) for keys with these hashes.
pub(crate) fn build(hashes: &[u64], bits_per_key: usize) -> Vec<u8> {
    let k = num_probes(bits_per_key);
    let bytes = hashes
        .len()
        .saturating_mul(bits_per_key)
        .div_ceil(8)
        .max(MIN_BYTES);
    let m_bits = bytes as u64 * 8;
    let mut out = vec![0u8; bytes + 1];
    for &h in hashes {
        for bit in probes(h, k, m_bits) {
            out[(bit / 8) as usize] |= 1 << (bit % 8);
        }
    }
    out[bytes] = k as u8;
    out
}

/// A filter block read back from a table.
#[derive(Debug)]
pub(crate) struct BloomFilter {
    bits: Vec<u8>,
    k: u32,
}

impl BloomFilter {
    /// Validate a filter block's contents (trailer already checked).
    pub(crate) fn new(mut contents: Vec<u8>) -> Result<BloomFilter> {
        let k = match contents.pop() {
            Some(k) => u32::from(k),
            None => return Err(Error::Corruption("empty bloom filter".into())),
        };
        if !(1..=MAX_PROBES).contains(&k) {
            return Err(Error::Corruption(format!("bloom filter has k = {k}")));
        }
        if contents.is_empty() {
            return Err(Error::Corruption("bloom filter has no bits".into()));
        }
        Ok(BloomFilter { bits: contents, k })
    }

    pub(crate) fn probes(&self) -> u32 {
        self.k
    }

    /// `false` means the key was certainly never added.
    pub(crate) fn may_contain(&self, key: &[u8]) -> bool {
        let m_bits = self.bits.len() as u64 * 8;
        probes(hash64(key), self.k, m_bits)
            .all(|bit| self.bits[(bit / 8) as usize] & (1 << (bit % 8)) != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn filter(keys: &[Vec<u8>], bpk: usize) -> BloomFilter {
        let hashes: Vec<u64> = keys.iter().map(|k| hash64(k)).collect();
        BloomFilter::new(build(&hashes, bpk)).unwrap()
    }

    #[test]
    fn hash_test_vectors() {
        assert_eq!(hash64(b""), 0xefd0_1f60_ba99_2926);
        assert_eq!(hash64(b"a"), 0x82a2_a958_a9be_ce5b);
        assert_eq!(hash64(b"strata"), 0x5a4f_965e_8a9b_0ac3);
    }

    #[test]
    fn probes() {
        assert_eq!(num_probes(10), 7);
        assert_eq!(num_probes(1), 1);
        assert_eq!(num_probes(0), 1);
        assert_eq!(num_probes(20), 14);
        assert_eq!(num_probes(100), 30);
    }

    #[test]
    fn false_positive_rate() {
        let keys: Vec<Vec<u8>> = (0..10_000)
            .map(|i| format!("key{i}").into_bytes())
            .collect();
        let f = filter(&keys, 10);
        let hits = (0..10_000)
            .filter(|i| f.may_contain(format!("miss{i}").as_bytes()))
            .count();
        let rate = hits as f64 / 10_000.0;
        assert!(rate < 0.02, "false-positive rate {rate}");
    }

    #[test]
    fn layout() {
        let hashes: Vec<u64> = (0..100u32).map(|i| hash64(&i.to_le_bytes())).collect();
        let b = build(&hashes, 10);
        assert_eq!(b.len(), 126);
        assert_eq!(*b.last().unwrap(), 7);
        assert_eq!(build(&[], 10).len(), 9);
    }

    #[test]
    fn rejects_bad_contents() {
        for bad in [
            vec![],
            vec![7],
            vec![0xff; 8].into_iter().chain([0]).collect(),
            {
                let mut v = vec![0xff; 8];
                v.push(31);
                v
            },
        ] {
            assert!(
                matches!(BloomFilter::new(bad.clone()), Err(Error::Corruption(_))),
                "{bad:?}"
            );
        }
    }

    proptest! {
        #[test]
        fn no_false_negatives(
            keys in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..16), 0..500),
            bpk in 1usize..=20,
        ) {
            let f = filter(&keys, bpk);
            for k in &keys {
                prop_assert!(f.may_contain(k));
            }
        }

        #[test]
        fn arbitrary_bytes_never_panic(
            bytes in proptest::collection::vec(any::<u8>(), 0..64),
            probe in proptest::collection::vec(any::<u8>(), 0..8),
        ) {
            if let Ok(f) = BloomFilter::new(bytes) {
                let _ = f.may_contain(&probe);
            }
        }
    }
}
