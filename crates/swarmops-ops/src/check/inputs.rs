//! A check's declared inputs (M30-3): the path patterns of its `inputs`, and the entries of a
//! tree they match.
//!
//! ```text
//!   inputs = ["docs/**", "README.md"] ─ parse ─▶ Inputs: sorted, none twice
//!                                                  │
//!       a path ─ Inputs::matches ─▶ whether one of the patterns matches it
//!       a tree ─ resolve ────────▶ Entries: (path, kind, blob id), each once, by path bytes
//!                resolve_one ────▶ the same for one pattern, walking below its names only
//!   no inputs ─ resolve ─────────▶ Whole: the tree itself is the input
//! ```
//!
//! The grammar is `access.toml`'s, read by the one matcher both share: `/`-separated segments,
//! each a name or `*` (one whole segment), the last one possibly `**` (every depth below, at
//! least one segment). A name must be one a repository path may hold. `docs/*` matches
//! `docs/a.md`, not `docs/a/b.md`; `docs/**` matches both, not `docs`.
//!
//! A pattern matches files, executables and symbolic links, a link by its own path and never
//! by what it points at. It matches no directory: `web` names an entry `web`, and matches
//! nothing in a tree where `web` is a directory.
//!
//! A tree is read through [`Tree`], one directory at a time: [`StoreTree`] reads a changeset's
//! tree from a store, [`HostTree`] the files below a directory of the caller's host. A walk
//! lists only the directories a pattern can match below.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt as _;

use camino::Utf8PathBuf;
use swarmops_api::{EntryKindOut, ErrorCode, OpError, ReadTreeReq, StoreOps};
use swarmops_core::RepoPath;

use super::file::quoted;
use super::{MAX_CHECK_INPUTS, MAX_INPUT_PATTERN_BYTES, host_failed};
use crate::error;
use crate::pattern::{DEEP, ONE, Pattern, segments_match};

/// The entries a [`StoreTree`] asks a store for at a time.
const STORE_PAGE: usize = 1_000;

/// The permission bits that make a host's file an executable.
const EXEC_BITS: u32 = 0o111;

/// The directory at a workspace's root that holds its own state, no part of its tree.
const WORKSPACE_DIR: &str = ".swarmws";

/// The directory git keeps a checkout's state in, no part of a tree at any depth.
const GIT_DIR: &str = ".git";

/// A check's `inputs`, parsed: its patterns sorted bytewise as written, none twice. None at all
/// is a check that declares no input: the whole tree is its input.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Inputs {
    /// The patterns, in the bytewise order of their text.
    patterns: Vec<Pattern>,
}

impl Inputs {
    /// How many patterns there are.
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// Whether there is no pattern: the whole tree is the input.
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether the repository path `path` is an input: one of the patterns matches it, or there
    /// is no pattern.
    pub fn matches(&self, path: &str) -> bool {
        self.patterns.is_empty() || self.patterns.iter().any(|pattern| pattern.matches(path))
    }
}

/// Reads `patterns` as a check's `inputs`, or says which one is at fault (its index) and what
/// is wrong with it, in words that follow the key's name: more than [`MAX_CHECK_INPUTS`] of
/// them (at the first past it), a pattern of more than [`MAX_INPUT_PATTERN_BYTES`] bytes, one
/// outside the grammar, one whose names no repository path may hold, or one given twice (at
/// the second). No pattern at all reads as the whole tree. The fuzz target's entry.
pub fn parse(patterns: &[String]) -> Result<Inputs, (usize, String)> {
    let mut seen = BTreeSet::new();
    let mut read: Vec<(&str, Pattern)> = Vec::with_capacity(patterns.len().min(MAX_CHECK_INPUTS));
    for (at, text) in patterns.iter().enumerate() {
        // 1. Within the count and the size.
        if at == MAX_CHECK_INPUTS {
            let what = format!("holding more than {MAX_CHECK_INPUTS} patterns");
            return Err((at, what));
        }
        if text.len() > MAX_INPUT_PATTERN_BYTES {
            let what = format!(
                "holds a pattern of {} bytes, more than {MAX_INPUT_PATTERN_BYTES}",
                text.len()
            );
            return Err((at, what));
        }

        // 2. Of the grammar, its names those of a repository path: the pattern with a name in
        //    place of each wildcard is one.
        let Some(pattern) = Pattern::parse(text) else {
            let what = format!(
                "holds {}, not a path pattern: segments between /, each a name or *, the last \
                 one possibly **",
                quoted(text)
            );
            return Err((at, what));
        };
        let named: Vec<&str> = (pattern.segments().iter())
            .map(|segment| if wild(segment) { "x" } else { segment.as_str() })
            .collect();
        if let Err(e) = RepoPath::new(named.join("/")) {
            let what = format!("holds {}, not a path pattern: {e}", quoted(text));
            return Err((at, what));
        }

        // 3. Once.
        if !seen.insert(text.as_str()) {
            return Err((at, format!("holds {} twice", quoted(text))));
        }
        read.push((text, pattern));
    }
    read.sort_by(|a, b| a.0.cmp(b.0));
    Ok(Inputs {
        patterns: read.into_iter().map(|(_, pattern)| pattern).collect(),
    })
}

/// One entry of a tree, as a [`Tree`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Entry {
    /// Its path from the tree's root, a repository path.
    pub path: String,
    /// What it is.
    pub kind: EntryKindOut,
    /// Its id in the store: a blob's for a file, an executable or a link, a tree's for a
    /// directory. None for an entry of a host's directory, which has no id until it is stored.
    pub id: Option<String>,
}

impl Entry {
    /// The entry of `kind` at `path`, with no id.
    pub fn new(path: impl Into<String>, kind: EntryKindOut) -> Entry {
        Entry {
            path: path.into(),
            kind,
            id: None,
        }
    }
}

/// A tree, read one directory at a time.
pub trait Tree {
    /// The entries of the directory `dir` (the root when none), each with its path from the
    /// root, directories among them, in any order. No entry when the tree holds no directory
    /// there: nothing, a file, or a link, which is never followed.
    fn list(&self, dir: Option<&str>) -> Result<Vec<Entry>, OpError>;
}

/// The tree of a changeset, read from a store through [`StoreOps::read_tree`], a page at a
/// time.
#[non_exhaustive]
pub struct StoreTree<'s> {
    /// The store.
    store: &'s dyn StoreOps,
    /// The changeset, by its full id, so that every page reads one tree.
    id: String,
    /// The entries asked of the store at a time: [`STORE_PAGE`], but in a test that lists a
    /// small directory over several pages.
    page: usize,
}

impl<'s> StoreTree<'s> {
    /// The tree of the changeset `id`, a full id as `resolve_rev` gives it, in `store`.
    pub fn new(store: &'s dyn StoreOps, id: impl Into<String>) -> StoreTree<'s> {
        StoreTree {
            store,
            id: id.into(),
            page: STORE_PAGE,
        }
    }
}

impl Tree for StoreTree<'_> {
    fn list(&self, dir: Option<&str>) -> Result<Vec<Entry>, OpError> {
        let mut entries = Vec::new();
        let mut cursor = None;
        loop {
            // 1. A page of the directory's own entries. Below the root, no directory there is
            //    no entry, not an error.
            let mut req = ReadTreeReq::new(self.id.as_str());
            req.path = dir.map(str::to_owned);
            req.depth = 1;
            req.limit = self.page;
            req.cursor = cursor.take();
            let page = match self.store.read_tree(req) {
                Ok(page) => page,
                Err(e) if dir.is_some() && e.code == ErrorCode::PathNotFound.as_str() => {
                    return Ok(Vec::new());
                }
                Err(e) => return Err(e),
            };

            // 2. Each with its path from the root; then the next page, if one follows.
            let more = page.truncated && !page.entries.is_empty();
            for entry in page.entries {
                let path = match dir {
                    Some(dir) => format!("{dir}/{}", entry.name),
                    None => entry.name,
                };
                entries.push(Entry {
                    path,
                    kind: entry.kind,
                    id: Some(entry.id),
                });
            }
            match page.cursor {
                Some(next) if more => cursor = Some(next),
                _ => return Ok(entries),
            }
        }
    }
}

/// The files below a directory of the caller's host, no link followed. Every entry there is
/// one of the tree's but `.swarmws` at the root, every `.git` directory, a name that is not
/// UTF-8 and an entry that is neither a file, a link nor a directory. No ignore file is read:
/// what `.gitignore` and `.swarmignore` keep out of a workspace's tree, build output for one,
/// is listed here, so a directory that holds such files resolves to more than the store's tree
/// of the same checkout. An entry has no id. Only the caller stands behind what the directory
/// holds.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HostTree {
    /// The directory.
    root: Utf8PathBuf,
}

impl HostTree {
    /// The tree of the files below `dir`.
    pub fn new(dir: impl Into<Utf8PathBuf>) -> HostTree {
        HostTree { root: dir.into() }
    }
}

impl Tree for HostTree {
    fn list(&self, dir: Option<&str>) -> Result<Vec<Entry>, OpError> {
        let shown = dir.unwrap_or(".");
        let gone =
            |e: &std::io::Error| matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory);

        // 1. The directory, each part of its path a directory itself, never a link, and none
        //    of them left out of the tree.
        let mut at = self.root.clone();
        if let Some(dir) = dir {
            if RepoPath::new(dir).is_err() {
                return Ok(Vec::new());
            }
            for (depth, part) in dir.split('/').enumerate() {
                if left_out(depth == 0, part, true) {
                    return Ok(Vec::new());
                }
                at.push(part);
                match std::fs::symlink_metadata(&at) {
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) => return Ok(Vec::new()),
                    Err(e) if gone(&e) => return Ok(Vec::new()),
                    Err(e) => return Err(host_failed(shown, &e)),
                }
            }
        }

        // 2. Its entries, each by what it is itself: a link is a link, whatever it points at.
        let listing = match std::fs::read_dir(&at) {
            Ok(listing) => listing,
            Err(e) if dir.is_some() && gone(&e) => return Ok(Vec::new()),
            Err(e) => return Err(host_failed(shown, &e)),
        };
        let mut entries = Vec::new();
        for entry in listing {
            let entry = entry.map_err(|e| host_failed(shown, &e))?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) if gone(&e) => continue,
                Err(e) => return Err(host_failed(shown, &e)),
            };
            let kind = if meta.file_type().is_symlink() {
                EntryKindOut::Symlink
            } else if meta.is_dir() {
                EntryKindOut::Dir
            } else if meta.is_file() {
                if meta.permissions().mode() & EXEC_BITS == 0 {
                    EntryKindOut::File
                } else {
                    EntryKindOut::Exec
                }
            } else {
                continue;
            };
            if left_out(dir.is_none(), &name, kind == EntryKindOut::Dir) {
                continue;
            }
            let path = match dir {
                Some(dir) => format!("{dir}/{name}"),
                None => name,
            };
            entries.push(Entry::new(path, kind));
        }
        Ok(entries)
    }
}

/// Whether a host's entry `name` is no part of the tree: `.swarmws` at the root, or a
/// directory `.git` anywhere.
fn left_out(at_root: bool, name: &str, is_dir: bool) -> bool {
    (at_root && name == WORKSPACE_DIR) || (is_dir && name == GIT_DIR)
}

/// What a check's inputs are in a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Resolved {
    /// The check declares no input: the whole tree, which no walk lists.
    Whole,
    /// The entries the patterns match.
    Entries {
        /// The files, executables and links matched, each once, in the bytewise order of
        /// their paths.
        entries: Vec<Entry>,
        /// Whether more entries match than the limit asked for: `entries` are then the first
        /// of them in that order.
        truncated: bool,
    },
}

/// The entries of `tree` that `inputs` match, at most `limit` of them: [`Resolved::Whole`] for
/// inputs of no pattern, else every pattern's matches in one list.
pub fn resolve(tree: &dyn Tree, inputs: &Inputs, limit: usize) -> Result<Resolved, OpError> {
    if inputs.is_empty() {
        return Ok(Resolved::Whole);
    }
    walk(tree, &inputs.patterns, None, limit)
}

/// The entries of `tree` that one pattern of `inputs` matches, at most `limit` of them: the
/// pattern at index `at` of the sorted patterns. Only what is below the names the pattern
/// starts with is listed, so a `limit` of 1 asks cheaply whether the pattern matches anything.
pub fn resolve_one(
    tree: &dyn Tree,
    inputs: &Inputs,
    at: usize,
    limit: usize,
) -> Result<Resolved, OpError> {
    let Some(pattern) = inputs.patterns.get(at) else {
        return Err(error::error(
            ErrorCode::Internal,
            format!("no input pattern {at} of {}", inputs.len()),
        ));
    };

    // The names before the first wildcard, the last segment aside: it names an entry, not a
    // directory to list.
    let segments = pattern.segments();
    let names = (segments.iter())
        .take(segments.len().saturating_sub(1))
        .take_while(|segment| !wild(segment))
        .count();
    let start = (names > 0).then(|| segments[..names].join("/"));
    walk(tree, std::slice::from_ref(pattern), start.as_deref(), limit)
}

/// The entries below `start` (the root when none) that one of `patterns` matches, in the
/// bytewise order of their paths, at most `limit` of them.
fn walk(
    tree: &dyn Tree,
    patterns: &[Pattern],
    start: Option<&str>,
    limit: usize,
) -> Result<Resolved, OpError> {
    let mut entries = Vec::new();
    let mut levels = vec![in_path_order(tree.list(start)?).into_iter()];
    while let Some(level) = levels.last_mut() {
        let Some(entry) = level.next() else {
            levels.pop();
            continue;
        };

        // 1. A directory: listed only if a pattern can match below it.
        if entry.kind == EntryKindOut::Dir {
            let names: Vec<&str> = entry.path.split('/').collect();
            if patterns.iter().any(|pattern| reaches(pattern, &names)) {
                levels.push(in_path_order(tree.list(Some(&entry.path))?).into_iter());
            }
            continue;
        }

        // 2. Anything else: taken if a pattern matches it, until the limit.
        if patterns.iter().any(|pattern| pattern.matches(&entry.path)) {
            if entries.len() == limit {
                return Ok(Resolved::Entries {
                    entries,
                    truncated: true,
                });
            }
            entries.push(entry);
        }
    }
    Ok(Resolved::Entries {
        entries,
        truncated: false,
    })
}

/// One directory's `entries` in the order a walk must take them in for the paths it finds to
/// come out sorted bytewise: by name, a directory's with the `/` that follows it in the paths
/// below it. So `a-b` comes before the directory `a`, as `a-b` sorts before `a/c`.
fn in_path_order(mut entries: Vec<Entry>) -> Vec<Entry> {
    entries.sort_by(|a, b| path_key(a).cmp(path_key(b)));
    entries
}

/// The bytes [`in_path_order`] sorts `entry` by: its path, and a `/` after a directory's.
fn path_key(entry: &Entry) -> impl Iterator<Item = u8> {
    let slash = (entry.kind == EntryKindOut::Dir).then_some(b'/');
    entry.path.bytes().chain(slash)
}

/// Whether `pattern` can match an entry below the directory whose path is the segments `dir`.
fn reaches(pattern: &Pattern, dir: &[&str]) -> bool {
    let segments = pattern.segments();
    match segments.split_last() {
        // A trailing `**`: the directory agrees with as much of what is before it as it has.
        Some((last, head)) if last == DEEP => segments_match(head, dir),
        // Otherwise a longer pattern whose start the directory matches.
        _ => segments.len() > dir.len() && segments_match(segments, dir),
    }
}

/// Whether `segment` of a pattern is a wildcard.
fn wild(segment: &str) -> bool {
    segment == ONE || segment == DEEP
}

#[cfg(test)]
mod tests;
