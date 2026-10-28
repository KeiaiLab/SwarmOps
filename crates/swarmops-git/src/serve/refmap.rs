//! The map between store refs and the git refs `serve.git` advertises (M14 design §3): `main` and
//! the branches under `refs/heads/`, the tags under `refs/tags/`, `refs/land/<target>` for a push
//! that lands. Remote-tracking refs are not served.
//!
//! ```text
//! store ref (format)        git ref
//! main (1, 2)               refs/heads/main          HEAD names it
//! release/<x> (1, 2)        refs/heads/release/<x>
//! <path> (2)                refs/heads/<path>        a branch
//! tags/<path> (2)           refs/tags/<path>         a lightweight tag
//! remote/<r>/<path> (2)     —                        Reserved
//! —                         refs/land/<target>       Land: push only
//! —                         any other refs/…         NotServed
//! ```
//!
//! A store name git does not take as a ref name is not served: the store grammar is git's but
//! for `release/<x>`, whose `x` (a FORMAT 1 rule) may hold `..` or end in `.` or `.lock`.
//! [`to_git`] gives `None` for it, and the refresh leaves it out of the advertisement with a
//! warning.

use swarmops_store::{RefKind, StoreFormat, ref_kind};

/// Where a branch is in git.
const HEADS: &str = "refs/heads/";

/// Where a tag is in git.
const TAGS: &str = "refs/tags/";

/// Where a push names the branch it lands onto.
const LAND: &str = "refs/land/";

/// Where a tag is in the store.
const STORE_TAGS: &str = "tags/";

/// The suffix no component of a git ref name ends in: git's lock files.
const LOCK_SUFFIX: &str = ".lock";

/// The name git keeps for the branch a repository has checked out, never a branch of ours.
const HEAD: &str = "HEAD";

/// What a git ref a client names stands for in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Mapped {
    /// A store ref, by its name and kind.
    Ref {
        /// The store ref's name: `main`, `release/<x>`, a branch or `tags/<path>`.
        store: String,
        /// Its kind.
        kind: RefKind,
    },
    /// `refs/land/<target>`: a push that lands onto the store branch `target`.
    Land {
        /// The branch to land onto.
        target: String,
    },
    /// No store ref: why.
    Refused(Refusal),
}

/// Why a git ref maps to no store ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// A name the store reserves (`remote/…`, `HEAD`).
    Reserved,
    /// A ref outside what `serve` serves.
    NotServed,
    /// A name the store's format does not take, and a later one does.
    FormatRequired {
        /// The format that takes it.
        needs: StoreFormat,
    },
}

/// The git ref the store ref `store` of kind `kind` is served as; `None` for one not served: a
/// remote-tracking ref, a name that is not of kind `kind`, the branch `HEAD`, and a name git does
/// not take (`release/a..b`). `to_git(RefKind::Tag, "tags/v1")` is `refs/tags/v1`.
pub fn to_git(kind: RefKind, store: &str) -> Option<String> {
    if ref_kind(store) != Some(kind) || store == HEAD {
        return None;
    }

    // The kind's place in git; a kind this build does not know is not served.
    let full = match kind {
        RefKind::Main | RefKind::Release | RefKind::Branch => format!("{HEADS}{store}"),
        RefKind::Tag => format!("{TAGS}{}", store.strip_prefix(STORE_TAGS)?),
        _ => return None,
    };
    git_takes(&full).then_some(full)
}

/// What the git ref `git_ref` stands for in a store of format `format`: the inverse of
/// [`to_git`] for a served ref, [`Mapped::Land`] for `refs/land/<branch>`, and the refusal for the
/// rest. `from_git(StoreFormat::V1, "refs/heads/feature/x")` is
/// `Refused(FormatRequired { needs: V2 })`.
pub fn from_git(format: StoreFormat, git_ref: &str) -> Mapped {
    if git_ref == HEAD {
        return Mapped::Refused(Refusal::Reserved);
    }
    if !git_takes(git_ref) {
        return Mapped::Refused(Refusal::NotServed);
    }

    // 1. A branch: its name must be one of the store's branch kinds.
    if let Some(name) = git_ref.strip_prefix(HEADS) {
        return match branch_kind(name) {
            Ok(kind) => store_ref(format, name, kind),
            Err(refusal) => Mapped::Refused(refusal),
        };
    }

    // 2. A tag: `tags/<path>` in the store.
    if let Some(path) = git_ref.strip_prefix(TAGS) {
        let store = format!("{STORE_TAGS}{path}");
        return match ref_kind(&store) {
            Some(RefKind::Tag) => store_ref(format, &store, RefKind::Tag),
            _ => Mapped::Refused(Refusal::NotServed),
        };
    }

    // 3. A land onto a branch; a land onto a tag or anything else is not served.
    if let Some(target) = git_ref.strip_prefix(LAND) {
        return match branch_kind(target) {
            Ok(kind) => match store_ref(format, target, kind) {
                Mapped::Ref { store, .. } => Mapped::Land { target: store },
                refused => refused,
            },
            Err(_) => Mapped::Refused(Refusal::NotServed),
        };
    }
    Mapped::Refused(Refusal::NotServed)
}

/// Whether git takes `full` as a ref name, by `git check-ref-format`'s rules: no component
/// empty, starting with `.` or ending in `.lock`; no `..`, `@{`, `\`, control character, space,
/// `~`, `^`, `:`, `?`, `*` or `[`; not ending in `.`; not `@`; at least two components.
fn git_takes(full: &str) -> bool {
    let forbidden = |c: char| c.is_ascii_control() || " ~^:?*[\\".contains(c);
    let component_ok =
        |part: &str| !part.is_empty() && !part.starts_with('.') && !part.ends_with(LOCK_SUFFIX);
    full != "@"
        && full.contains('/')
        && !full.ends_with('.')
        && !full.contains("..")
        && !full.contains("@{")
        && !full.contains(forbidden)
        && full.split('/').all(component_ok)
}

/// The kind of the store name `name` a branch or a land target takes: `main`, `release/<x>` or a
/// branch; a tag, a remote-tracking ref, `HEAD` or a name under a reserved first segment is
/// [`Refusal::Reserved`], a name the store grammar refuses [`Refusal::NotServed`].
fn branch_kind(name: &str) -> Result<RefKind, Refusal> {
    match ref_kind(name) {
        Some(kind @ (RefKind::Main | RefKind::Release)) => Ok(kind),
        Some(RefKind::Branch) if name != HEAD => Ok(RefKind::Branch),
        Some(_) => Err(Refusal::Reserved),
        None if reserved_first(name) => Err(Refusal::Reserved),
        None => Err(Refusal::NotServed),
    }
}

/// Whether `name`'s first segment is one the store reserves: `main/x`, `tags`, `remote/x`.
fn reserved_first(name: &str) -> bool {
    let first = name.split('/').next().unwrap_or_default();
    matches!(first, "main" | "release" | "tags" | "remote")
}

/// The store ref `name` of kind `kind` in a store of `format`: refused when only a later format
/// takes the kind.
fn store_ref(format: StoreFormat, name: &str, kind: RefKind) -> Mapped {
    let needs = kind.needs();
    if needs.version() > format.version() {
        return Mapped::Refused(Refusal::FormatRequired { needs });
    }
    Mapped::Ref {
        store: name.to_owned(),
        kind,
    }
}
