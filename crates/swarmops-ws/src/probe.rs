//! A queue runner's probes (the M22 core design §5 steps 3 and 7): which prefixes of a round's
//! chain to check, and what their verdicts decide.
//!
//! ```text
//!  chain   1   2   3   4   5   6   7   8        g = the longest green prefix
//!          ✓       ✓   ?   ?   ✗   ?   ?        r = the shortest red one, else the chain's end
//!                  g ───── (g, r] ─────r
//!
//!  r open, r-g <= w   every prefix of (g, r]          (all candidates at once)
//!  r open, r-g >  w   w spaced ones ending at r       g + ⌈i(r-g)/w⌉,     i = 1..w
//!  r red              w inside (g, r)                 g + ⌈i(r-g)/(w+1)⌉, i = 1..w
//! ```
//!
//! With `w` 1 a red end is split in its middle: a binary search for the entry to blame (C4).

/// What the verdicts say of one prefix of a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Known {
    /// No decisive verdict: none yet, or one cancelled from outside.
    Open,
    /// Its check passed.
    Green,
    /// Its check failed or ran past its timeout.
    Red,
}

/// Whether the end of the undecided interval is a red prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum End {
    /// Undecided: the chain's end, no red prefix known.
    Open,
    /// A red prefix.
    Red,
}

/// The undecided interval `(green, end]` of a chain, as its verdicts leave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Interval {
    /// The longest green prefix shorter than `end`: 0 when none.
    pub green: u32,
    /// The shortest red prefix, else the chain's length.
    pub end: u32,
    /// Whether `end` is red.
    pub red: End,
}

/// The interval `known`, the chain's prefixes from 1 in order, leaves undecided.
///
/// `[✓, ?, ✓, ?, ✗, ✗]` leaves `(3, 5]` with 5 red; `[?, ✓, ?, ?]` leaves `(2, 4]` open.
pub fn decide(known: &[Known]) -> Interval {
    // 1. The end: the shortest red prefix, else the whole chain.
    let red = known.iter().position(|k| *k == Known::Red);
    let (end, red) = match red {
        Some(at) => (at + 1, End::Red),
        None => (known.len(), End::Open),
    };

    // 2. The longest green prefix shorter than it.
    let green = (known[..end.saturating_sub(usize::from(red == End::Red))].iter())
        .rposition(|k| *k == Known::Green)
        .map_or(0, |at| at + 1);
    Interval {
        green: count(green),
        end: count(end),
        red,
    }
}

/// The prefixes to check next in `(green, end]`, at most `width` of them, in increasing order.
///
/// `select(0, 8, End::Open, 4)` is `[2, 4, 6, 8]`; `select(0, 8, End::Red, 1)` is `[4]`.
pub fn select(green: u32, end: u32, red: End, width: u32) -> Vec<u32> {
    let width = width.max(1);
    let span = end.saturating_sub(green);

    // 1. An open end: every prefix when they fit, else `width` spaced ones ending at it.
    if red == End::Open {
        if span <= width {
            return (green + 1..=end).collect();
        }
        return (1..=width)
            .map(|i| green + (i * span).div_ceil(width))
            .collect();
    }

    // 2. A red end: `width` inside it, none twice, none at it.
    let mut inner: Vec<u32> = (1..=width)
        .map(|i| green + (i * span).div_ceil(width + 1))
        .filter(|at| *at > green && *at < end)
        .collect();
    inner.dedup();
    inner
}

/// A chain's length or place as a `u32`: a chain holds at most 32 changes.
fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
