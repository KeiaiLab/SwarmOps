//! SwarmOps' merge algorithms: pure functions over trees and lines, with no I/O and no store
//! (storage spec §4). M2 brings the tree diff (the leaf-level changes between two trees, their
//! canonical delta and the three-way path entries) and the line diff (the histogram hunks of
//! `imara-diff`); M4 adds the three-way merge on top of them. The callers hand it the trees they
//! hold: the repository engine re-exports this crate as `swarmops_repo::merge`, and M3's
//! importer diffs the trees it has just built in memory.

pub mod line_diff;

#[cfg(test)]
mod tests {
    use imara_diff::{Algorithm, Diff, Hunk, InternedInput};

    /// The line diff storage §4 cites, pinned: `imara-diff` 0.2's histogram diff over interned
    /// lines, whose hunks are 0-based, half-open `u32` line ranges of each side.
    #[test]
    fn a_histogram_diff_finds_one_changed_line() {
        let input = InternedInput::new("a\nb\nc\n", "a\nx\nc\n");
        let diff = Diff::compute(Algorithm::Histogram, &input);
        let hunks: Vec<Hunk> = diff.hunks().collect();
        assert_eq!(
            hunks,
            [Hunk {
                before: 1_u32..2,
                after: 1_u32..2
            }]
        );
    }
}
