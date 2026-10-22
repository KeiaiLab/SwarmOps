//! What the workspace tests and measurements share (M5): the synthetic tree of the M5 design's
//! §9 (50,000 files in 2,500 directories) and the random-edit driver over a plain directory,
//! std and `tempfile` only, with no dependency on `swarmops-ws`.
//!
//! ```text
//!   root/
//!     d0000/f00000.txt  f02500.txt  f05000.txt  …    file i is in directory i % dirs
//!     d0001/f00001.txt  f02501.txt  …
//!     …
//! ```
//!
//! Every file of a given version has the same length, so a rewrite keeps the size: the edit a
//! status that trusts size and mtime alone would miss.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Files of the §9 tree.
pub const TREE_FILES: usize = 50_000;

/// Directories of the §9 tree.
pub const TREE_DIRS: usize = 2_500;

/// The length of every file's content, whatever its version.
const CONTENT_LEN: usize = 64;

/// The digits of a version in a file's content: versions below 10^6 keep the length.
const VERSION_DIGITS: usize = 6;

/// The repository path of file `i` in a tree of `dirs` directories.
pub fn path_of(i: usize, dirs: usize) -> String {
    format!("d{:04}/f{i:05}.txt", i % dirs)
}

/// The content of file `i` at `version`: 64 bytes, so that two versions differ in
/// content and never in size.
pub fn content_of(i: usize, version: u32) -> String {
    let head = format!("file {i:08} version {version:0VERSION_DIGITS$}\n");
    format!("{head:.<width$}\n", width = CONTENT_LEN - 1)
}

/// Writes the synthetic tree of `files` files in `dirs` directories below `root` (made if
/// missing), every file at version 0. Returns the repository paths, in file order.
pub fn write_tree(root: &Path, files: usize, dirs: usize) -> io::Result<Vec<String>> {
    assert!(dirs > 0, "a tree needs a directory");
    let mut paths = Vec::with_capacity(files);
    for i in 0..files {
        let path = path_of(i, dirs);
        let file = root.join(&path);
        fs::create_dir_all(file.parent().expect("a file has a directory"))?;
        fs::write(&file, content_of(i, 0))?;
        paths.push(path);
    }
    Ok(paths)
}

/// A small deterministic generator (SplitMix64): a seed fixes the edits a test makes, so that a
/// failure reruns.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator from `seed`.
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number below `n` (`n > 0`).
    pub fn below(&mut self, n: usize) -> usize {
        let n64 = u64::try_from(n).expect("a usize fits a u64");
        usize::try_from(self.next_u64() % n64).expect("below n")
    }
}

/// One edit [`random_edits`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// The file rewritten with new content of the same size.
    Rewrite(String),
    /// The file removed.
    Remove(String),
    /// A file added that the tree lacks.
    Add(String),
    /// The file replaced by another file renamed over it (a different inode, same path).
    RenameOver(String),
}

impl Edit {
    /// The path the edit touched.
    pub fn path(&self) -> &str {
        match self {
            Edit::Rewrite(p) | Edit::Remove(p) | Edit::Add(p) | Edit::RenameOver(p) => p,
        }
    }
}

/// Makes `n` random edits to the tree below `root` that [`write_tree`] wrote (`files` files, in
/// `dirs` directories), each on a file not yet touched, and returns them in the order made. A
/// rewrite or a rename-over gives the file version `version`, so the content is new and the size
/// the same. `n` is at most `files`.
pub fn random_edits(
    root: &Path,
    (files, dirs): (usize, usize),
    version: u32,
    rng: &mut Rng,
    n: usize,
) -> io::Result<Vec<Edit>> {
    assert!(n <= files, "more edits than files");

    // 1. n distinct files, picked by a partial shuffle of their numbers.
    let mut numbers: Vec<usize> = (0..files).collect();
    for k in 0..n {
        let j = k + rng.below(files - k);
        numbers.swap(k, j);
    }

    // 2. One edit each: a rewrite, a removal, an addition beside it, or a rename-over.
    let mut done = Vec::with_capacity(n);
    for &i in &numbers[..n] {
        let path = path_of(i, dirs);
        let file: PathBuf = root.join(&path);
        let edit = match rng.below(4) {
            0 => {
                fs::write(&file, content_of(i, version))?;
                Edit::Rewrite(path)
            }
            1 => {
                fs::remove_file(&file)?;
                Edit::Remove(path)
            }
            2 => {
                let added = format!("d{:04}/added{i:05}.txt", i % dirs);
                fs::write(root.join(&added), content_of(i, version))?;
                Edit::Add(added)
            }
            _ => {
                let temp = file.with_extension("renamed");
                fs::write(&temp, content_of(i, version))?;
                fs::rename(&temp, &file)?;
                Edit::RenameOver(path)
            }
        };
        done.push(edit);
    }
    Ok(done)
}

/// Rewrites the first `n` files of the tree (`dirs` directories) with version `version`: the same
/// size, new content.
pub fn rewrite_first(root: &Path, dirs: usize, n: usize, version: u32) -> io::Result<()> {
    for i in 0..n {
        fs::write(root.join(path_of(i, dirs)), content_of(i, version))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Contents of one file at two versions differ and have the same length; the path puts file
    /// `i` in directory `i % dirs`.
    #[test]
    fn versions_keep_the_size_and_change_the_bytes() {
        let (a, b) = (content_of(7, 0), content_of(7, 1));
        assert_ne!(a, b);
        assert_eq!(a.len(), b.len());
        assert_eq!(a.len(), CONTENT_LEN);
        assert_eq!(path_of(2_501, TREE_DIRS), "d0001/f02501.txt");
    }

    /// A tree of 30 files in 6 directories is written whole; random edits touch distinct files,
    /// the same seed makes the same edits, and a rewrite keeps its file's size.
    #[test]
    fn a_seed_fixes_the_edits_and_a_rewrite_keeps_the_size() {
        let run = |seed: u64| {
            let tmp = tempfile::tempdir().unwrap();
            let paths = write_tree(tmp.path(), 30, 6).unwrap();
            assert_eq!(paths.len(), 30);
            let edits = random_edits(tmp.path(), (30, 6), 1, &mut Rng::new(seed), 12).unwrap();
            for edit in &edits {
                match edit {
                    Edit::Rewrite(p) | Edit::RenameOver(p) => {
                        let len = fs::metadata(tmp.path().join(p)).unwrap().len();
                        assert_eq!(len, CONTENT_LEN as u64, "{p}");
                    }
                    Edit::Remove(p) => assert!(!tmp.path().join(p).exists(), "{p}"),
                    Edit::Add(p) => assert!(tmp.path().join(p).exists(), "{p}"),
                }
            }
            edits
        };
        let first = run(5);
        assert_eq!(first, run(5));
        assert_ne!(first, run(6));
        let mut touched: Vec<&str> = first.iter().map(Edit::path).collect();
        touched.sort_unstable();
        touched.dedup();
        assert_eq!(touched.len(), 12);
    }
}
