//! A check's declared inputs (M30-3): the path patterns of its `inputs`, in the access file's
//! grammar (`Pattern`), from the tree's root.
//!
//! ```text
//!   inputs = ["web/**", "docs/*", "CHANGELOG.md"]
//!
//!   web/**         web/app/page.tsx, web/package.json      every depth below web
//!   docs/*         docs/guide.md                           not docs/status/M1/a.txt
//!   CHANGELOG.md   CHANGELOG.md                            that entry alone
//!
//!   no `inputs`    every path: the check takes the whole tree
//! ```
//!
//! A pattern names paths as they are, byte for byte, whatever the check's `dir`. A path matches
//! a check's inputs when any pattern does; its *first match* is the first one in declared order.
//! [`resolve`] gives the entries of a changeset's tree that a check's inputs match, and
//! [`resolve_paths`] the same match over a list of paths.

use swarmops_api::{EntryKindOut, OpError, ReadTreeReq, StoreOps};

use super::{MAX_CHECK_INPUTS, MAX_INPUT_PATTERN_BYTES};
use crate::pattern::{DEEP, Pattern};

/// A check's inputs, read: its patterns in declared order. None is the whole tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    /// The patterns, in declared order.
    patterns: Vec<Pattern>,
}

impl Inputs {
    /// The inputs `patterns` declare, or the index of the first one at fault and what is wrong
    /// with it: more than [`MAX_CHECK_INPUTS`] of them, one that is no pattern (`rule`), or
    /// one given twice (at the second). No pattern reads as the whole tree.
    pub fn parse(patterns: &[String]) -> Result<Inputs, (usize, String)> {
        let mut read = Vec::with_capacity(patterns.len().min(MAX_CHECK_INPUTS));
        for (at, text) in patterns.iter().enumerate() {
            if at == MAX_CHECK_INPUTS {
                return Err((at, format!("more than {MAX_CHECK_INPUTS} patterns")));
            }
            let pattern = rule(text).map_err(|what| (at, what))?;
            if patterns[..at].contains(text) {
                return Err((at, "given twice".to_owned()));
            }
            read.push(pattern);
        }
        Ok(Inputs { patterns: read })
    }

    /// Whether it holds no pattern: the check takes the whole tree.
    pub fn whole_tree(&self) -> bool {
        self.patterns.is_empty()
    }

    /// The index, in declared order, of the first pattern `path` matches. None when none
    /// does, and for the whole tree, which has no pattern to name.
    pub fn first_match(&self, path: &str) -> Option<usize> {
        (self.patterns.iter()).position(|pattern| pattern.matches(path))
    }

    /// Whether `path` is among the inputs: a pattern matches it, or there is none.
    pub fn matches(&self, path: &str) -> bool {
        self.whole_tree() || self.first_match(path).is_some()
    }

    /// Whether a path below the directory `dir` could be among the inputs: a pattern reaches
    /// below it, or there is none.
    fn reach_below(&self, dir: &str) -> bool {
        let segments: Vec<&str> = dir.split('/').collect();
        self.whole_tree()
            || (self.patterns.iter()).any(|pattern| pattern.could_match_below(&segments))
    }
}

/// The entries of a tree that a check's inputs match.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Resolved {
    /// The paths of the matched entries, from the tree's root, in tree order: directory by
    /// directory, a directory's entries by name, bytewise.
    pub entries: Vec<String>,
    /// Whether more entries match than `entries` holds: the walk stopped at its limit. A caller
    /// that needs every entry and gets `truncated` must fail closed: take the check's inputs as
    /// not known, never as the entries listed.
    pub truncated: bool,
}

/// The entries of changeset `id`'s tree that `inputs` match, at most `limit` of them, in tree
/// order. An entry is a file, an executable or a symbolic link; a directory is none, so `docs`
/// matches a file named `docs` and nothing below a directory of that name. No pattern gives
/// every entry.
///
/// ```text
///   read_tree(id, dir, depth 1), every page ─▶ each entry, by name:
///       a file, an executable, a link ─ a pattern matches its path ─▶ listed
///       a directory ─ a pattern could match below it ─▶ walked next, before its siblings
///                   └ none could ─▶ never read
/// ```
///
/// Reads the store and writes nothing. A store error is the error; a tree deeper than
/// [`MAX_TREE_DEPTH`](swarmops_api::MAX_TREE_DEPTH) is the store's own refusal.
pub fn resolve(
    store: &dyn StoreOps,
    id: &str,
    inputs: &Inputs,
    limit: usize,
) -> Result<Resolved, OpError> {
    let mut walk = Walk {
        store,
        id,
        inputs,
        limit,
        found: Resolved {
            entries: Vec::new(),
            truncated: false,
        },
    };
    walk.directory(None)?;
    Ok(walk.found)
}

/// The paths of `paths` that `inputs` match, in the order given: [`resolve`]'s match over a list
/// of paths, not a tree.
pub fn resolve_paths<'p>(inputs: &Inputs, paths: impl Iterator<Item = &'p str>) -> Vec<&'p str> {
    paths.filter(|path| inputs.matches(path)).collect()
}

/// The most entries of a directory read at once.
const TREE_PAGE: usize = 1_000;

/// A walk of one tree for [`resolve`].
struct Walk<'w> {
    /// The store read.
    store: &'w dyn StoreOps,
    /// The changeset whose tree is walked, a full id.
    id: &'w str,
    /// The inputs matched.
    inputs: &'w Inputs,
    /// The most entries to list.
    limit: usize,
    /// What matched so far.
    found: Resolved,
}

impl Walk<'_> {
    /// Walks the directory at `dir` (none: the root), every page of it, and the directories
    /// below it that a pattern can reach. `false` once the limit stopped the walk.
    fn directory(&mut self, dir: Option<&str>) -> Result<bool, OpError> {
        let mut cursor = None;
        loop {
            // 1. One page of the directory's own entries, in the tree's order.
            let mut req = ReadTreeReq::new(self.id);
            req.path = dir.map(str::to_owned);
            req.limit = TREE_PAGE;
            req.cursor = cursor;
            let page = self.store.read_tree(req)?;

            // 2. Each entry: a directory walked if a pattern reaches below it, any other
            //    entry listed if one matches it.
            for entry in &page.entries {
                let path = match dir {
                    Some(dir) => format!("{dir}/{}", entry.name),
                    None => entry.name.clone(),
                };
                if entry.kind == EntryKindOut::Dir {
                    if self.inputs.reach_below(&path) && !self.directory(Some(&path))? {
                        return Ok(false);
                    }
                    continue;
                }
                if !self.inputs.matches(&path) {
                    continue;
                }
                if self.found.entries.len() == self.limit {
                    self.found.truncated = true;
                    return Ok(false);
                }
                self.found.entries.push(path);
            }

            // 3. The next page, if any.
            cursor = page.cursor.filter(|_| page.truncated);
            if cursor.is_none() {
                return Ok(true);
            }
        }
    }
}

/// The pattern `text`, or what is wrong with it: more than [`MAX_INPUT_PATTERN_BYTES`] bytes, a
/// NUL, a segment `.` or `..` (which no repository path holds), or one of the grammar's own
/// faults: an empty segment, a `*` beside other bytes, a `**` before the last segment.
fn rule(text: &str) -> Result<Pattern, String> {
    if text.len() > MAX_INPUT_PATTERN_BYTES {
        return Err(format!(
            "of {} bytes, more than {MAX_INPUT_PATTERN_BYTES}",
            text.len()
        ));
    }
    if text.contains('\0') {
        return Err("not a pattern: a NUL byte".to_owned());
    }
    let mut segments = text.split('/');
    if segments
        .clone()
        .any(|segment| segment == "." || segment == "..")
    {
        return Err("not a pattern: a segment . or .., which no path holds".to_owned());
    }
    if let Some(pattern) = Pattern::parse(text) {
        return Ok(pattern);
    }

    // The grammar's own fault, named: the first that holds.
    let why = if segments.clone().any(str::is_empty) {
        "an empty segment"
    } else if segments.any(|segment| segment.contains('*') && segment != "*" && segment != DEEP) {
        "a * beside other bytes in a segment"
    } else {
        "a ** before the last segment"
    };
    Err(format!("not a pattern: {why}"))
}

#[cfg(test)]
mod tests;
