//! A log histogram of 32 buckets an octave (the M21 design §2.3): its memory is bounded by the
//! values' range, never by their number.
//!
//! ```text
//!   bucket of v ≥ 1 = floor(32 · log2 v) + 1      (0 holds v = 0)
//!   each bucket keeps its count, least and most value seen
//!   a percentile = the middle of [least, most] of the bucket that holds its rank
//! ```
//!
//! A bucket's most is below 2^(1/32) ≈ 1.0219 times its least, so the middle is within
//! 1.1 % of any value in it.

use std::collections::BTreeMap;

/// The bits of the fraction of `log2 v` a bucket resolves: 2^5 = 32 buckets an octave.
const FRACTION_BITS: u32 = 5;

/// The fixed point of a mantissa in `[1, 2)`: 63 bits of fraction.
const ONE_SHIFT: u32 = 63;

/// One bucket: how many values fell in it, the least and the most.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    count: u64,
    least: u64,
    most: u64,
}

/// The values of one figure in one window.
#[derive(Debug, Clone, Default)]
pub(super) struct Histogram {
    buckets: BTreeMap<u32, Bucket>,
    count: u64,
}

impl Histogram {
    /// Adds `value`.
    pub(super) fn add(&mut self, value: u64) {
        let bucket = self.buckets.entry(index(value)).or_insert(Bucket {
            count: 0,
            least: value,
            most: value,
        });
        bucket.count += 1;
        bucket.least = bucket.least.min(value);
        bucket.most = bucket.most.max(value);
        self.count += 1;
    }

    /// The `percent` percentile, by nearest rank; none without a value.
    pub(super) fn percentile(&self, percent: u64) -> Option<u64> {
        // 1. The rank: the least whose share of the values reaches `percent`.
        if self.count == 0 {
            return None;
        }
        let rank = (self.count * percent).div_ceil(100).max(1);

        // 2. The bucket that holds it, and its middle.
        let mut below = 0;
        for bucket in self.buckets.values() {
            below += bucket.count;
            if below >= rank {
                return Some(bucket.least + (bucket.most - bucket.least) / 2);
            }
        }
        None
    }
}

/// The bucket of `value`: 0 for 0, else `floor(32 · log2 value) + 1`, worked out on integers
/// (the octave from the leading zeros, the fraction by squaring the mantissa).
fn index(value: u64) -> u32 {
    // 1. Zero apart; the octave, and the mantissa in [1, 2) as a fixed point.
    if value == 0 {
        return 0;
    }
    let octave = u64::BITS - 1 - value.leading_zeros();
    let mut mantissa = u128::from(value) << (ONE_SHIFT - octave);

    // 2. Each squaring doubles the logarithm: past 2, one more bit of its fraction.
    let mut fraction = 0;
    for _ in 0..FRACTION_BITS {
        mantissa = (mantissa * mantissa) >> ONE_SHIFT;
        fraction <<= 1;
        if mantissa >> (ONE_SHIFT + 1) != 0 {
            fraction |= 1;
            mantissa >>= 1;
        }
    }
    (octave << FRACTION_BITS) + fraction + 1
}

#[cfg(test)]
mod tests {
    use super::{Histogram, index};

    /// Each value's bucket is the floor of 32 times its logarithm, plus one.
    #[test]
    fn a_bucket_is_32_an_octave() {
        assert_eq!(index(0), 0);
        assert_eq!(index(1), 1);
        assert_eq!(index(2), 33);
        assert_eq!(index(1 << 20), 20 * 32 + 1);
        assert_eq!(index(u64::MAX), 63 * 32 + 31 + 1);
        // 1,024 · 2^(1/32) ≈ 1,046.4: the first bucket of that octave ends there.
        assert_eq!((index(1_024), index(1_046), index(1_047)), (321, 321, 322));
    }

    /// Every percentile is within 1.1 % of the exact one.
    #[test]
    fn a_percentile_is_within_1_1_percent() {
        let values: Vec<u64> = (1..=10_000u64).map(|i| i * i).collect();
        let mut histogram = Histogram::default();
        for &v in &values {
            histogram.add(v);
        }
        for percent in [1u64, 50, 90, 99, 100] {
            let exact = values[usize::try_from((10_000 * percent).div_ceil(100) - 1).unwrap()];
            let read = histogram.percentile(percent).unwrap();
            assert!(
                read.abs_diff(exact) * 1_000 <= exact * 11,
                "p{percent}: {read} for {exact}"
            );
        }
        assert_eq!(Histogram::default().percentile(50), None);
    }
}
