//! git's verbs on the command line (the git-familiar command design, §2 to §5): one table,
//! [`ROWS`], from which a line clap refuses is read again as the SwarmOps line of the same
//! meaning ([`parse`]), and a refused git verb gets one line that names what to type
//! ([`hint`]).
//!
//! ```text
//! argv ─▶ cli::parse ─▶ Run | Text: as today
//!            └▶ Usage ─▶ expand ─▶ changed? cli::parse again ─▶ Run | Text | Usage(+ hint)
//! ```
//!
//! A line that parses today never reaches [`expand`], so no existing line changes meaning. The
//! layer does no I/O: where the line was typed is the caller's `place`, asked only when a rule
//! needs it.

use std::cell::Cell;
use std::ffi::OsString;

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Command, CommandFactory as _};
use swarmops_api::remote::DEFAULT_REMOTE;

use crate::cli::{self, Cli, Parsed};

/// Where a line was typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// At a workspace's root.
    Root,
    /// Below a workspace's root.
    Below,
    /// In no workspace.
    Elsewhere,
}

/// The audit's tier of a git verb: the everyday set, or the one scripts and CI use.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    One,
    Two,
}

/// How a git verb maps to SwarmOps (the design's §2).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// The same verb does the same today.
    Same,
    /// The same verb, through an alias of this layer.
    Alias,
    /// Different on purpose: the verb is refused with the hint.
    Different,
    /// No such concept: the hint says what to do.
    NoConcept,
    /// A candidate for later.
    Later,
}

/// Lines for the user to type, as words, without the program's name.
pub(crate) type Lines = &'static [&'static [&'static str]];

/// One git verb: what to type instead, and the line a refusal of it carries.
pub(crate) struct Row {
    /// git's verb.
    pub(crate) git: &'static str,
    /// The SwarmOps lines that do what it does.
    pub(crate) lines: Lines,
    /// The first line of a refusal's hint: `{so}` is the program's name, `{0}`… a line of
    /// `lines` with it. `None` for a verb that is a command or always expands.
    pub(crate) hint: Option<&'static str>,
    #[cfg(test)]
    pub(crate) tier: Tier,
    #[cfg(test)]
    pub(crate) classes: &'static [Class],
    /// The envelope's `op` of `example`, for a verb that runs.
    #[cfg(test)]
    pub(crate) op: Option<&'static str>,
    /// A line that shows the row, as words.
    #[cfg(test)]
    pub(crate) example: &'static [&'static str],
    #[cfg(test)]
    pub(crate) note: &'static str,
    /// What would have to be built, for a candidate for later.
    #[cfg(test)]
    pub(crate) later: Option<&'static str>,
}

/// A [`Row`], written once: the fields only the page and the tests read exist only in tests.
macro_rules! row {
    (
        $git:literal, $tier:ident, [$($class:ident),+],
        lines: [$([$($word:literal),*]),*],
        hint: $hint:expr,
        op: $op:expr,
        example: [$($example:literal),*],
        note: $note:literal,
        later: $later:expr $(,)?
    ) => {
        Row {
            git: $git,
            lines: &[$(&[$($word),*]),*],
            hint: $hint,
            #[cfg(test)]
            tier: Tier::$tier,
            #[cfg(test)]
            classes: &[$(Class::$class),+],
            #[cfg(test)]
            op: $op,
            #[cfg(test)]
            example: &[$($example),*],
            #[cfg(test)]
            note: $note,
            #[cfg(test)]
            later: $later,
        }
    };
}

/// git's verbs in daily use, in the audit's order: tier 1, then tier 2.
pub(crate) const ROWS: &[Row] = &[
    row!("init", One, [Same],
        lines: [["init", "DIR"], ["ws", "create", "NAME"]],
        hint: None, op: Some("repo_init"), example: ["init", "store2"],
        note: "Makes a store, no working tree; `DIR` is required. Then `so ws create NAME`. \
               `-b`, `--bare` are not taken.",
        later: None),
    row!("clone", One, [Alias, Later],
        lines: [["clone", "URL", "DIR"], ["ws", "create", "NAME"]],
        hint: None, op: Some("clone_repo"),
        example: ["clone", "https://remote.invalid/x.git", "copy"],
        note: "A: positional `DIR` (= `--into`). A store and no files, as `git clone --bare`; \
               then `so ws create NAME`.",
        later: Some("`--ws NAME`, a new flag: a working tree in the same step")),
    row!("status", One, [Alias],
        lines: [["status"]],
        hint: None, op: Some("ws_status"), example: ["status"],
        note: "`-s`, `-b`, `-sb`, `--short`, `--branch` ask for the text listing. Paths from \
               the workspace's root.",
        later: None),
    row!("add", One, [NoConcept],
        lines: [["commit", "-m", "MSG", "[PATH]..."]],
        hint: Some("git add: there is no staging area; {0} in a workspace takes every change, \
                    or the named paths"),
        op: None, example: ["add", "a.txt"],
        note: "No staging area: a commit takes every change, or the named paths.",
        later: None),
    row!("rm", One, [NoConcept],
        lines: [["commit", "-m", "MSG"]],
        hint: Some("git rm: delete the file in the workspace; {0} records it"),
        op: None, example: ["rm", "a.txt"],
        note: "Delete the file in the workspace; the next commit records it.",
        later: None),
    row!("mv", One, [NoConcept],
        lines: [["commit", "-m", "MSG"]],
        hint: Some("git mv: move the file in the workspace; {0} records it"),
        op: None, example: ["mv", "a.txt", "b.txt"],
        note: "Move the file in the workspace; the next commit records a delete and an add.",
        later: None),
    row!("commit", One, [Alias],
        lines: [["commit", "-m", "MSG", "[PATH]..."]],
        hint: None, op: Some("ws_commit"), example: ["commit", "-m", "x"],
        note: "`so commit --changes FILE` keeps its meaning (the JSON change list). Takes every \
               change, new files included unless an ignore file names them (git's `-a` leaves \
               new files out); `-a` is accepted. Paths from the workspace's root. No \
               `--amend` (D: `so op undo`, then commit again), no editor.",
        later: None),
    row!("diff", One, [Same, Alias],
        lines: [["diff", "FROM", "TO"], ["diff"]],
        hint: None, op: Some("ws_diff"), example: ["diff"],
        note: "Lists the changed paths by default; `-p` for the patch. `--stat`, \
               `--name-status`, `-U<n>`. Paths by `--path P`, not `-- P`, from the \
               workspace's root. No `A..B`, no `--staged`.",
        later: None),
    row!("log", One, [Same, Alias],
        lines: [["log", "[REV]", "-n", "N", "--path", "P"]],
        hint: None, op: Some("log"), example: ["log", "-1"],
        note: "First parents, from `main` unless REV is given, also inside a workspace: its \
               own commits are `so log ID` with the head `so ws info` gives. A: `--oneline`, \
               `-<n>`. No `--graph`, `--author`, `--since`, ranges.",
        later: None),
    row!("show", One, [Same],
        lines: [["show", "REV"], ["cat", "REV:PATH"]],
        hint: None, op: Some("show"), example: ["show", "main"],
        note: "The changeset and its changed paths, no patch. `git show REV:PATH` is \
               `so cat REV:PATH`.",
        later: None),
    row!("branch", One, [Different, Later],
        lines: [
            ["ref", "list"],
            ["ref", "set", "NAME", "REV", "--create"],
            ["ref", "delete", "NAME", "--expect", "ID"]
        ],
        hint: Some("git branch: {0} lists refs; {1} makes one; {2} removes one"),
        op: None, example: ["branch"],
        note: "A ref list also holds `tags/` and `remote/`; there is no current branch.",
        later: Some("a `branch` verb once FORMAT 2 branches map to git refs (with M11)")),
    row!("switch", One, [NoConcept, Later],
        lines: [["ws", "create", "NAME", "--base", "REV"]],
        hint: Some("git switch: a workspace does not switch; {0} makes another"),
        op: None, example: ["switch", "main"],
        note: "A workspace never switches; make another.",
        later: Some("unscheduled")),
    row!("checkout", One, [NoConcept],
        lines: [["ws", "create", "NAME", "--base", "REV"], ["restore", "PATH..."]],
        hint: Some("git checkout: {0} for another tree; {1} to drop edits"),
        op: None, example: ["checkout", "main"],
        note: "Not offered under this name (git itself split it).",
        later: None),
    row!("restore", One, [Alias],
        lines: [["restore", "PATH..."], ["ws", "discard"]],
        hint: Some("git restore: name the paths, {0}; {1} drops every edit"),
        op: Some("ws_discard"), example: ["restore", "a.txt"],
        note: "At least one path, as in git. A file never committed is deleted (git leaves an \
               untracked file). Paths from the workspace's root; below the root the line is \
               refused. `.` is no path (exit 2). No `--staged`, `--source`. Every edit: \
               `so ws discard`.",
        later: None),
    row!("merge", One, [Different],
        lines: [["ws", "land"], ["land", "HEAD"]],
        hint: Some("git merge: SwarmOps lands; {0} from a workspace, or {1}"),
        op: None, example: ["merge", "main"],
        note: "One squashed changeset onto the target by compare-and-swap; no merge commit.",
        later: None),
    row!("rebase", One, [Same, Alias],
        lines: [["rebase", "HEAD", "--onto", "REF"], ["rebase"]],
        hint: None, op: Some("ws_rebase"), example: ["rebase"],
        note: "Store form: one squashed changeset, no ref moves. Workspace form: the workspace \
               follows its target; `--continue`, `--abort`, `--onto` there. No `-i`.",
        later: None),
    row!("fetch", One, [Same, Alias],
        lines: [["fetch", "REMOTE"], ["fetch"]],
        hint: None, op: Some("fetch"), example: ["fetch"],
        note: "`main` never moves on a fetch. No `--all`, `--prune`.",
        later: None),
    row!("pull", One, [Different],
        lines: [["sync", "REMOTE", "--no-push"], ["rebase"]],
        hint: Some("git pull: {0} brings the remote into main; {1} in a workspace follows it"),
        op: None, example: ["pull"],
        note: "`sync` lands onto the remote's head and moves `main`; a workspace follows by its \
               own rebase.",
        later: None),
    row!("push", One, [Same, Alias],
        lines: [["push", "REMOTE", "[REF[:BRANCH]]"], ["push"]],
        hint: None, op: Some("push"), example: ["push"],
        note: "Sends `main` unless REF is named; `main` and `release/*` only. \
               Compare-and-swap, fast-forward only; no `--force`, `-u`, `--tags`.",
        later: None),
    row!("remote", One, [Same, Alias],
        lines: [
            ["remote", "add", "NAME", "URL"],
            ["remote", "remove", "NAME"],
            ["remote", "list"],
            ["remote"]
        ],
        hint: None, op: Some("remote_list"), example: ["remote", "-v"],
        note: "`so remote`, `so remote -v` list; `so remote rm` removes. No `rename`, \
               `set-url`.",
        later: None),
    row!("tag", One, [Alias, Different],
        lines: [
            ["tag"],
            ["ref", "set", "tags/NAME", "REV", "--create"],
            ["ref", "delete", "tags/NAME", "--expect", "ID"]
        ],
        hint: Some("git tag: {0} lists; {1} makes one; {2} removes one"),
        op: Some("ref_list"), example: ["tag"],
        note: "FORMAT 2 stores. Lightweight only; a tag never moves.",
        later: None),
    row!("stash", One, [Different, Later],
        lines: [["ws", "snapshot"]],
        hint: Some("git stash: {0} records the edits and leaves the files; nothing pops"),
        op: None, example: ["stash"],
        note: "Records the edits as a changeset and leaves the files; nothing pops.",
        later: Some("unscheduled")),
    row!("reset", One, [Different],
        lines: [["ws", "discard", "[PATH]..."], ["op", "undo", "OP"]],
        hint: Some("git reset: {0} drops edits; {1} takes an operation back; {so} ref set \
                    moves a ref"),
        op: None, example: ["reset"],
        note: "`main` moves forward only. A ref: `so ref set NAME REV --expect ID`.",
        later: None),
    row!("revert", One, [Same],
        lines: [["revert", "REV", "--reason", "TEXT"]],
        hint: None, op: Some("revert"), example: ["revert", "main", "--reason", "x"],
        note: "Lands the inverse; `--since` for a range. The reason is required.",
        later: None),
    row!("cherry-pick", One, [NoConcept, Later],
        lines: [["land", "HEAD", "--base", "BASE"]],
        hint: Some("git cherry-pick: not built; {0} lands one change's range"),
        op: None, example: ["cherry-pick", "main"],
        note: "One change's range lands as one changeset.",
        later: Some("M24 (rewriting with lineage)")),
    row!("blame", One, [Later],
        lines: [["history", "REV:PATH"]],
        hint: Some("git blame: not built yet (M7); {0} lists the changesets that touched a \
                    path"),
        op: None, example: ["blame", "a.txt"],
        note: "The changesets that touched a path.",
        later: Some("M7")),
    row!("grep", One, [NoConcept],
        lines: [["cat", "REV:PATH"]],
        hint: Some("git grep: search the files of a workspace; {0} reads one at a revision"),
        op: None, example: ["grep", "x"],
        note: "`rg` or `grep` in a workspace.",
        later: None),
    row!("config", One, [NoConcept],
        lines: [["whoami"], ["remote"]],
        hint: Some("git config: identity is --as or SWARMOPS_AGENT ({0}); the store is --repo \
                    or SWARMOPS_REPO; remotes are {1}"),
        op: None, example: ["config", "user.name"],
        note: "There is no configuration command.",
        later: None),
    row!("reflog", One, [Alias],
        lines: [["reflog", "NAME"], ["op", "log"]],
        hint: Some("git reflog: {0} lists a ref's moves; {1} every operation; nothing expires"),
        op: Some("ref_log"), example: ["reflog", "main"],
        note: "A name is required (no store-level HEAD). Every operation: `so op log`. \
               `expire`, `delete`, `exists` are refused with the hint: nothing expires.",
        later: None),
    row!("rev-parse", Two, [Different],
        lines: [["resolve", "REV"], ["ws", "info"]],
        hint: Some("git rev-parse: {0} gives the full id; {1} a workspace's head and path"),
        op: None, example: ["rev-parse", "main"],
        note: "",
        later: None),
    row!("rev-list", Two, [Different],
        lines: [["log", "REV", "-n", "N"], ["ancestry", "FROM", "TO"]],
        hint: Some("git rev-list: {0}; {1}"),
        op: None, example: ["rev-list", "main"],
        note: "",
        later: None),
    row!("ls-files", Two, [Different],
        lines: [["ls", "REV[:PATH]"]],
        hint: Some("git ls-files: {0}"),
        op: None, example: ["ls-files"],
        note: "A workspace's files are plain files.",
        later: None),
    row!("ls-tree", Two, [Different],
        lines: [["ls", "REV[:PATH]", "--depth", "N"]],
        hint: Some("git ls-tree: {0}"),
        op: None, example: ["ls-tree", "main"],
        note: "JSON entries, not `<mode> <type> <id>\\t<path>`.",
        later: None),
    row!("cat-file", Two, [Different],
        lines: [["cat", "REV:PATH"], ["blob", "get", "ID"], ["blob", "has", "ID"]],
        hint: Some("git cat-file: {0}; {1}; {2}"),
        op: None, example: ["cat-file", "-p", "main"],
        note: "",
        later: None),
    row!("hash-object", Two, [Different],
        lines: [["blob", "put", "FILE"]],
        hint: Some("git hash-object: {0}"),
        op: None, example: ["hash-object", "a.txt"],
        note: "",
        later: None),
    row!("describe", Two, [NoConcept],
        lines: [["tag"]],
        hint: Some("git describe: not built; {0} lists the tags"),
        op: None, example: ["describe"],
        note: "Unscheduled.",
        later: None),
    row!("for-each-ref", Two, [Different],
        lines: [["ref", "list", "[--prefix P]"]],
        hint: Some("git for-each-ref: {0}"),
        op: None, example: ["for-each-ref"],
        note: "",
        later: None),
    row!("show-ref", Two, [Different],
        lines: [["ref", "list"]],
        hint: Some("git show-ref: {0}"),
        op: None, example: ["show-ref"],
        note: "",
        later: None),
    row!("symbolic-ref", Two, [NoConcept],
        lines: [["ws", "info"]],
        hint: Some("git symbolic-ref: there are no symbolic refs; {0} gives a workspace's head \
                    and target"),
        op: None, example: ["symbolic-ref", "HEAD"],
        note: "No symbolic refs; no store-level HEAD.",
        later: None),
    row!("update-ref", Two, [Different],
        lines: [["ref", "set", "NAME", "REV", "--expect", "ID"]],
        hint: Some("git update-ref: {0}, or --create; {so} ref delete; {so} ref txn"),
        op: None, example: ["update-ref", "main", "main"],
        note: "",
        later: None),
    row!("merge-base", Two, [Same],
        lines: [["merge-base", "REV", "ONTO"], ["ancestry", "FROM", "TO"]],
        hint: None, op: Some("merge_base"), example: ["merge-base", "main", "main"],
        note: "Along first parents, with what REV changed. `--is-ancestor`: \
               `so ancestry FROM TO`.",
        later: None),
    row!("name-rev", Two, [NoConcept],
        lines: [["ref", "list"]],
        hint: Some("git name-rev: not built; {0} gives each ref's changeset"),
        op: None, example: ["name-rev", "main"],
        note: "",
        later: None),
    row!("ls-remote", Two, [Different],
        lines: [
            ["fetch", "REMOTE"],
            ["ref", "list", "--prefix", "remote/REMOTE/"],
            ["sync", "status", "REMOTE"]
        ],
        hint: Some("git ls-remote: {0}, then {1}; {2}"),
        op: None, example: ["ls-remote"],
        note: "",
        later: None),
    row!("diff-tree", Two, [Different],
        lines: [["diff", "FROM", "TO", "--format", "names"], ["show", "REV"]],
        hint: Some("git diff-tree: {0}; {1}"),
        op: None, example: ["diff-tree", "main"],
        note: "",
        later: None),
    row!("worktree", Two, [Alias, Different],
        lines: [
            ["worktree", "list"],
            ["ws", "create", "NAME", "[--path DIR]", "[--base REV]"],
            ["ws", "list"],
            ["ws", "delete", "NAME"]
        ],
        hint: Some("git worktree: {1}; {2}; {3}"),
        op: Some("ws_list"), example: ["worktree", "list"],
        note: "git names a path, SwarmOps a workspace name.",
        later: None),
    row!("clean", Two, [NoConcept],
        lines: [["ws", "discard", "PATH..."]],
        hint: Some("git clean: {0} drops the named paths' edits, new files included; without a \
                    path, every edit"),
        op: None, example: ["clean", "-f"],
        note: "",
        later: None),
    row!("archive", Two, [NoConcept, Later],
        lines: [["git", "export", "PATH"]],
        hint: Some("git archive: git init --bare PATH, {0}, then git archive there"),
        op: None, example: ["archive", "main"],
        note: "",
        later: Some("M25 (archives)")),
    row!("bisect", Two, [NoConcept, Later],
        lines: [],
        hint: Some("git bisect: not built"),
        op: None, example: ["bisect", "start"],
        note: "",
        later: Some("unscheduled")),
    row!("shortlog", Two, [NoConcept],
        lines: [["log"]],
        hint: Some("git shortlog: {0}, counted by author_agent"),
        op: None, example: ["shortlog"],
        note: "",
        later: None),
];

/// A hint of a refused line rather than of a word: its text, as [`Row::hint`]'s, and the lines
/// it names.
struct Said {
    text: &'static str,
    lines: Lines,
}

/// The prefix of a line that carries paths below a workspace's root.
const BELOW: &str = "paths are read from the workspace's root, not this directory: {0}";

/// `restore P...` below the root.
const BELOW_RESTORE: Said = Said {
    text: BELOW,
    lines: &[&["ws", "discard", "ROOT/RELATIVE/PATH"]],
};

/// `commit -m MSG P...` below the root.
const BELOW_COMMIT: Said = Said {
    text: BELOW,
    lines: &[&["ws", "commit", "-m", "MSG", "ROOT/RELATIVE/PATH"]],
};

/// `status P...` below the root.
const BELOW_STATUS: Said = Said {
    text: BELOW,
    lines: &[&["ws", "status", "ROOT/RELATIVE/PATH"]],
};

/// `diff --path P` below the root.
const BELOW_DIFF: Said = Said {
    text: BELOW,
    lines: &[&["ws", "diff", "ROOT/RELATIVE/PATH"]],
};

/// `diff` without revisions, outside a workspace.
const BARE_DIFF: Said = Said {
    text: "in a workspace this is {0}; {1} makes one",
    lines: &[&["ws", "diff"], &["ws", "create", "NAME"]],
};

/// `rebase` without a head, outside a workspace or with a store-only flag.
const BARE_REBASE: Said = Said {
    text: "in a workspace this is {0}; {1} makes one",
    lines: &[&["ws", "rebase"], &["ws", "create", "NAME"]],
};

/// `commit` without a change list, outside a workspace or without a message.
const BARE_COMMIT: Said = Said {
    text: "{0} in a workspace commits its changes; {1} makes one",
    lines: &[
        &["commit", "-m", "MSG", "[PATH]..."],
        &["ws", "create", "NAME"],
    ],
};

/// `clone URL` without a directory.
const BARE_CLONE: Said = Said {
    text: "name the directory: {0}",
    lines: &[&["clone", "URL", "DIR"]],
};

/// `diff` with one positional, in a workspace.
const ONE_REVISION: Said = Said {
    text: "a path is {0}; two revisions are {1}",
    lines: &[&["diff", "--path", "P"], &["diff", "FROM", "TO"]],
};

/// `commit`'s flags that `ws commit` lacks: a line with one is the store's commit.
const COMMIT_ONLY: [&str; 5] = [
    "--changes",
    "--parent",
    "--allow-empty",
    "--advance",
    "--key",
];

/// `rebase`'s flags that `ws rebase` lacks: a line with one is the store's rebase.
const REBASE_ONLY: [&str; 4] = ["--base", "--key", "--max-conflict-bytes", "--resolve"];

/// The words after `reflog` that are git's subcommands, not a ref: nothing expires.
const REFLOG_VERBS: [&str; 3] = ["expire", "delete", "exists"];

/// The flags that name a workspace.
const WS_FLAGS: [&str; 2] = ["--ws", "--ws-path"];

/// The flag that asks for text.
const HUMAN: &str = "--human";

/// The word after which every word is positional.
const DASHES: &str = "--";

// The commands a rule reads a line with and expands it to, as paths.
const WS_STATUS: &[&str] = &["ws", "status"];
const WS_DISCARD: &[&str] = &["ws", "discard"];
const WS_DIFF: &[&str] = &["ws", "diff"];
const WS_COMMIT: &[&str] = &["ws", "commit"];
const WS_REBASE: &[&str] = &["ws", "rebase"];
const WS_LIST: &[&str] = &["ws", "list"];
const REF_LOG: &[&str] = &["ref", "log"];
const REF_LIST: &[&str] = &["ref", "list"];
const REMOTE_LIST: &[&str] = &["remote", "list"];
const FETCH: &[&str] = &["fetch"];
const PUSH: &[&str] = &["push"];
const CLONE: &[&str] = &["clone"];
const LOG: &[&str] = &["log"];
const DIFF: &[&str] = &["diff"];
const COMMIT: &[&str] = &["commit"];
const REBASE: &[&str] = &["rebase"];
const REMOTE: &[&str] = &["remote"];

/// `tag`'s expansion: the refs under `tags/`.
const TAGS: &[&str] = &["ref", "list", "--prefix", "tags/"];

/// `cli::parse`; on `Usage`, [`expand`]; if the words changed, `cli::parse` of the expansion.
pub(crate) fn parse(args: &[OsString], place: &dyn Fn() -> Place) -> Parsed {
    let parsed = cli::parse(args);
    if !matches!(parsed, Parsed::Usage { .. }) {
        return parsed;
    }
    let expanded = expand(args, place);
    if expanded == args {
        return parsed;
    }
    cli::parse(&expanded)
}

/// `args` (the program's name first) with a built-in alias expanded; `args` itself otherwise.
/// Called only from [`parse`].
fn expand(args: &[OsString], place: &dyn Fn() -> Place) -> Vec<OsString> {
    let Some(words) = words(args) else {
        return args.to_vec();
    };
    let Plan::Into(new) = plan(&words, &Here::new(place)) else {
        return args.to_vec();
    };

    // The program's name as it was typed, then the expansion.
    args.iter()
        .take(1)
        .cloned()
        .chain(new.into_iter().map(OsString::from))
        .collect()
}

/// The line to put first in a usage error's hint, `{so}` filled with `bin`. `args` and `place`
/// are the line as typed: the below-the-root lines and the missing-set lines read them.
pub(crate) fn hint(
    error: &clap::Error,
    path: &[String],
    bin: Option<&str>,
    args: &[OsString],
    place: &dyn Fn() -> Place,
) -> Option<String> {
    let grammar = Cli::command();
    let so = bin.unwrap_or(grammar.get_name());
    let words = words(args)?;
    let here = Here::new(place);

    // 1. A line that carries paths, typed below a workspace's root.
    if let Plan::Below(said) = plan(&words, &here) {
        return Some(render(said.text, said.lines, so));
    }

    // 2. A git verb in the verb's place that is no command: its row's line.
    if error.kind() == ErrorKind::InvalidSubcommand && path.is_empty() {
        let Some(ContextValue::String(word)) = error.get(ContextKind::InvalidSubcommand) else {
            return None;
        };
        let row = ROWS.iter().find(|row| row.git == word)?;
        return Some(render(row.hint?, row.lines, so));
    }

    // 3. A bare form, refused for what it lacks.
    let said = missing(error, path, &words, &here)?;
    Some(render(said.text, said.lines, so))
}

/// The hint of a line `cli::parse` refused for a missing argument, `words` as typed: `diff`,
/// `rebase` or `commit` in their bare forms, `diff` with one revision in a workspace, `clone`
/// without a directory.
fn missing(
    error: &clap::Error,
    path: &[String],
    words: &[String],
    here: &Here<'_>,
) -> Option<&'static Said> {
    if error.kind() != ErrorKind::MissingRequiredArgument {
        return None;
    }
    let Some(ContextValue::Strings(lacking)) = error.get(ContextKind::InvalidArg) else {
        return None;
    };
    let lacks = |name: &str| {
        let with_value = format!("{name} ");
        (lacking.iter()).any(|arg| arg == name || arg.starts_with(&with_value))
    };
    let [verb] = path else {
        return None;
    };

    // The words after the verb, read with its flags.
    let grammar = built();
    let at = verb_at(words, &grammar)?;
    let flags = Flags::of(&grammar, &[&[verb.as_str()]]);
    let read = Read::new(words[at + 1..].to_vec(), &flags);

    match verb.as_str() {
        "diff" if lacks("<FROM>") && lacks("<TO>") => Some(&BARE_DIFF),
        "diff" if lacks("<TO>") && (read.names_ws() || here.get() != Place::Elsewhere) => {
            Some(&ONE_REVISION)
        }
        "rebase" if lacks("<HEAD>") => Some(&BARE_REBASE),
        "commit" if lacks("--changes") && !read.holds_flag(&COMMIT_ONLY) => Some(&BARE_COMMIT),
        "clone" if lacks("--into") && read.positionals().count() == 1 => Some(&BARE_CLONE),
        _ => None,
    }
}

/// `text` with `{so}` the program's name `so` and `{n}` the `n`th of `lines` after it.
fn render(text: &str, lines: Lines, so: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            rest = &rest[open..];
            break;
        };

        // `{so}`, `{0}`…; anything else is left as it is.
        let name = &after[..close];
        let line = name.parse::<usize>().ok().and_then(|at| lines.get(at));
        match (name, line) {
            ("so", _) => out.push_str(so),
            (_, Some(line)) => {
                out.push_str(so);
                out.push(' ');
                out.push_str(&line.join(" "));
            }
            (_, None) => out.push_str(&rest[open..=open + close + 1]),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// The words of `args` after the program's name, if every one is UTF-8.
fn words(args: &[OsString]) -> Option<Vec<String>> {
    let mut words = args
        .iter()
        .skip(1)
        .map(|arg| arg.to_str().map(str::to_owned));
    words.try_fold(Vec::new(), |mut all, word| {
        all.push(word?);
        Some(all)
    })
}

/// Where the line was typed: asked of the caller once, and only when a rule needs it.
struct Here<'a> {
    place: &'a dyn Fn() -> Place,
    known: Cell<Option<Place>>,
}

impl<'a> Here<'a> {
    fn new(place: &'a dyn Fn() -> Place) -> Self {
        Here {
            place,
            known: Cell::new(None),
        }
    }

    fn get(&self) -> Place {
        if let Some(place) = self.known.get() {
            return place;
        }
        let place = (self.place)();
        self.known.set(Some(place));
        place
    }
}

/// What the layer makes of a line.
enum Plan {
    /// Nothing: the line stays as it is.
    Keep,
    /// These words instead of the line's, after the program's name.
    Into(Vec<String>),
    /// Nothing: the line carries paths and was typed below a workspace's root.
    Below(&'static Said),
}

/// What the layer makes of `words`, the line after the program's name: the verb's rule, its
/// flag rules first, each once.
fn plan(words: &[String], here: &Here<'_>) -> Plan {
    let grammar = built();
    let Some(at) = verb_at(words, &grammar) else {
        return Plan::Keep;
    };

    // 1. The verb's rule reads the words after it.
    let line = Line {
        grammar: &grammar,
        here,
        human: holds(words, HUMAN),
        rest: &words[at + 1..],
    };
    let planned = match words[at].as_str() {
        "status" => line.status(),
        "restore" => line.restore(),
        "reflog" => line.reflog(),
        "worktree" => line.worktree(),
        "tag" => line.tag(),
        "remote" => line.remote(),
        "fetch" => line.origin(FETCH),
        "push" => line.origin(PUSH),
        "clone" => line.clone_repo(),
        "log" => line.log(),
        "diff" => line.diff(),
        "commit" => line.commit(),
        "rebase" => line.rebase(),
        _ => Plan::Keep,
    };

    // 2. The global flags before the verb stay where they were.
    match planned {
        Plan::Into(new) => Plan::Into(words[..at].iter().cloned().chain(new).collect()),
        other => other,
    }
}

/// The grammar, built: each command holds the global flags and `--help`.
fn built() -> Command {
    let mut root = Cli::command();
    root.build();
    root
}

/// Where the verb is in `words`: the first word that is neither a global flag nor its value.
/// `None` for a line with no verb, or with `--` or a flag the program lacks before it.
fn verb_at(words: &[String], grammar: &Command) -> Option<usize> {
    let flags = Flags::of(grammar, &[]);
    let mut value = false;
    for (at, word) in words.iter().enumerate() {
        if value {
            value = false;
            continue;
        }
        if word == DASHES {
            return None;
        }
        if !is_flag(word) {
            return Some(at);
        }
        value = flags.takes_next(word)?;
    }
    None
}

/// Whether `flag` is among `words` before a `--`.
fn holds(words: &[String], flag: &str) -> bool {
    (words.iter())
        .take_while(|word| *word != DASHES)
        .any(|word| word == flag)
}

/// Whether `word`, in a flag's place, is a flag: `-` alone is a positional.
fn is_flag(word: &str) -> bool {
    word.starts_with('-') && word != "-"
}

/// The words of `path`, then `words`: the expansion of a rule.
fn into(path: &[&str], words: Vec<String>) -> Plan {
    let path = path.iter().map(|&word| word.to_owned());
    Plan::Into(path.chain(words).collect())
}

/// `path` with `words` if a flag rule changed them; the line as it is otherwise.
fn flagged(changed: bool, path: &[&str], words: Vec<String>) -> Plan {
    if changed {
        into(path, words)
    } else {
        Plan::Keep
    }
}

/// One flag: its names, and whether it takes the next word as its value.
struct Flag {
    long: Option<String>,
    short: Option<char>,
    next: bool,
}

/// The flags a line's words are read with.
struct Flags(Vec<Flag>);

impl Flags {
    /// The global flags of `grammar` and the flags of the commands `paths` name in it.
    fn of(grammar: &Command, paths: &[&[&str]]) -> Self {
        let named = paths.iter().filter_map(|path| {
            (path.iter()).try_fold(grammar, |cmd, name| cmd.find_subcommand(name))
        });
        let flags = std::iter::once(grammar)
            .chain(named)
            .flat_map(Command::get_arguments)
            .filter(|arg| !arg.is_positional())
            .map(|arg| Flag {
                long: arg.get_long().map(str::to_owned),
                short: arg.get_short(),
                next: arg.get_num_args().is_some_and(|n| n.takes_values())
                    && !arg.is_require_equals_set(),
            });
        Flags(flags.collect())
    }

    /// Whether `word`, a flag, takes the next word as its value: not when it holds its value
    /// (`--name=V`, `-nV`). `None` for a flag none of these is.
    fn takes_next(&self, word: &str) -> Option<bool> {
        if let Some(long) = word.strip_prefix("--") {
            let (name, attached) = long
                .split_once('=')
                .map_or((long, false), |(name, _)| (name, true));
            let flag = self
                .0
                .iter()
                .find(|flag| flag.long.as_deref() == Some(name))?;
            return Some(flag.next && !attached);
        }
        let mut chars = word.chars().skip(1);
        let short = chars.next()?;
        let flag = self.0.iter().find(|flag| flag.short == Some(short))?;
        Some(flag.next && chars.next().is_none())
    }
}

/// What a word of a line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Flag,
    /// The value of the flag before it.
    Value,
    Positional,
    /// `--`: every word after it is a positional.
    Dashes,
}

/// The words of a line after its verb, each with its kind. A flag the commands lack takes no
/// value.
struct Read {
    words: Vec<String>,
    kinds: Vec<Kind>,
}

impl Read {
    fn new(words: Vec<String>, flags: &Flags) -> Self {
        let mut kinds = Vec::with_capacity(words.len());
        let mut value = false;
        for word in &words {
            let kind = if kinds.contains(&Kind::Dashes) {
                Kind::Positional
            } else if value {
                Kind::Value
            } else if word == DASHES {
                Kind::Dashes
            } else if is_flag(word) {
                Kind::Flag
            } else {
                Kind::Positional
            };
            value = kind == Kind::Flag && flags.takes_next(word).unwrap_or(false);
            kinds.push(kind);
        }
        Read { words, kinds }
    }

    /// The places of the positionals.
    fn positionals(&self) -> impl Iterator<Item = usize> + '_ {
        (self.kinds.iter())
            .enumerate()
            .filter(|(_, kind)| **kind == Kind::Positional)
            .map(|(at, _)| at)
    }

    /// The first positional, if any: its place and word.
    fn first(&self) -> Option<(usize, &str)> {
        let at = self.positionals().next()?;
        Some((at, self.words[at].as_str()))
    }

    fn has_positional(&self) -> bool {
        self.first().is_some()
    }

    /// The words with the one at `at` removed.
    fn without_at(mut self, at: usize) -> Vec<String> {
        self.words.remove(at);
        self.words
    }

    /// The words without the flags named `flags`.
    fn without(self, flags: &[&str]) -> Vec<String> {
        let words = self.words.into_iter().zip(self.kinds);
        words
            .filter(|(word, kind)| *kind != Kind::Flag || !flags.contains(&word.as_str()))
            .map(|(word, _)| word)
            .collect()
    }

    /// Whether a flag of the line is one of `names`, as `--name` or `--name=V`.
    fn holds_flag(&self, names: &[&str]) -> bool {
        self.flags().any(|word| {
            let name = word.split_once('=').map_or(word, |(name, _)| name);
            names.contains(&name)
        })
    }

    /// Whether the line names a workspace (`--ws`, `--ws-path`).
    fn names_ws(&self) -> bool {
        self.holds_flag(&WS_FLAGS)
    }

    /// Whether the line carries a commit message (`-m`, `-mMSG`, `--message`).
    fn holds_message(&self) -> bool {
        self.holds_flag(&["--message"]) || self.flags().any(|word| word.starts_with("-m"))
    }

    /// The flags of the line.
    fn flags(&self) -> impl Iterator<Item = &str> {
        (self.words.iter().zip(&self.kinds))
            .filter(|(_, kind)| **kind == Kind::Flag)
            .map(|(word, _)| word.as_str())
    }

    /// The words without each `--path P` (`--path=P`), and the `P`s.
    fn paths(self) -> (Vec<String>, Vec<String>) {
        let mut words = Vec::new();
        let mut paths = Vec::new();
        let mut taking = false;
        for (word, kind) in self.words.into_iter().zip(self.kinds) {
            // 1. The value of the `--path` before it.
            if taking && kind == Kind::Value {
                taking = false;
                paths.push(word);
                continue;
            }
            taking = false;
            if kind != Kind::Flag {
                words.push(word);
                continue;
            }

            // 2. `--path P` and `--path=P`; every other flag stays.
            if word == "--path" {
                taking = true;
                continue;
            }
            match word.strip_prefix("--path=") {
                Some(path) => paths.push(path.to_owned()),
                None => words.push(word),
            }
        }
        (words, paths)
    }
}

/// Whether a git flag asks for text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asks {
    Text,
    Nothing,
}

/// A flag rule: what a git flag becomes, and whether it asks for text; `None` for a word it
/// leaves.
type Rewrite = fn(&str) -> Option<(Vec<String>, Asks)>;

/// `status`'s shape flags (git's `-s`, `-b`, `-sb`): text.
fn status_flags(word: &str) -> Option<(Vec<String>, Asks)> {
    let shape = matches!(word, "-s" | "--short" | "-b" | "--branch" | "-sb" | "-bs");
    shape.then(|| (Vec::new(), Asks::Text))
}

/// `log`'s git flags: `--oneline` is text, `-<n>` is `-n <n>`.
fn log_flags(word: &str) -> Option<(Vec<String>, Asks)> {
    if word == "--oneline" {
        return Some((Vec::new(), Asks::Text));
    }
    let count = word.strip_prefix('-').filter(|count| is_number(count))?;
    Some((vec!["-n".to_owned(), count.to_owned()], Asks::Nothing))
}

/// `diff`'s shape flags: `--stat`, `--name-status`, `-p` (`-u`, `--patch`), `-U<n>`
/// (`--unified=<n>`), each a `--format` and text.
fn diff_flags(word: &str) -> Option<(Vec<String>, Asks)> {
    let format = |format: &str| vec!["--format".to_owned(), format.to_owned()];
    let words = match word {
        "--stat" => format("stat"),
        "--name-status" => format("names"),
        "-p" | "-u" | "--patch" => format("unified"),
        _ => {
            let lines = (word.strip_prefix("-U"))
                .or_else(|| word.strip_prefix("--unified="))
                .filter(|lines| is_number(lines))?;
            let mut words = format("unified");
            words.extend(["--context".to_owned(), lines.to_owned()]);
            words
        }
    };
    Some((words, Asks::Text))
}

/// `commit`'s git flags: `-a` (`--all`) is what `ws commit` does anyway, `-am M` is `-m M`.
fn commit_flags(word: &str) -> Option<(Vec<String>, Asks)> {
    if matches!(word, "-a" | "--all") {
        return Some((Vec::new(), Asks::Nothing));
    }
    let message = word.strip_prefix("-am")?;
    Some((vec![format!("-m{message}")], Asks::Nothing))
}

/// Whether `text` is a number: digits only.
fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// A line's verb rule at work: the words after the verb, and what it asks of the line.
struct Line<'a> {
    grammar: &'a Command,
    here: &'a Here<'a>,
    /// Whether the line holds `--human`.
    human: bool,
    rest: &'a [String],
}

impl Line<'_> {
    /// `words`, read with the flags of the commands `paths` name.
    fn read(&self, words: Vec<String>, paths: &[&[&str]]) -> Read {
        Read::new(words, &Flags::of(self.grammar, paths))
    }

    /// The words after the verb with `rewrite`'s flags replaced, and one `--human` after the
    /// first that asks for text if the line holds none; `None` if no flag was replaced.
    fn rewritten(&self, paths: &[&[&str]], rewrite: Rewrite) -> Option<Vec<String>> {
        let read = self.read(self.rest.to_vec(), paths);
        let mut out = Vec::with_capacity(read.words.len() + 1);
        let mut changed = false;
        let mut human = self.human;
        for (word, kind) in read.words.into_iter().zip(read.kinds) {
            let new = if kind == Kind::Flag {
                rewrite(&word)
            } else {
                None
            };
            let Some((words, asks)) = new else {
                out.push(word);
                continue;
            };
            changed = true;
            out.extend(words);
            if asks == Asks::Text && !human {
                out.push(HUMAN.to_owned());
                human = true;
            }
        }
        changed.then_some(out)
    }

    /// Whether the line is in a workspace: it names one, or was typed in one.
    fn inside(&self, read: &Read) -> bool {
        read.names_ws() || self.here.get() != Place::Elsewhere
    }

    /// For a line that carries paths: `None` where it expands (it names the workspace, or was
    /// typed at its root), the plan for it otherwise. A path is never guessed.
    fn refused(&self, named: bool, below: &'static Said) -> Option<Plan> {
        if named {
            return None;
        }
        match self.here.get() {
            Place::Root => None,
            Place::Below => Some(Plan::Below(below)),
            Place::Elsewhere => Some(Plan::Keep),
        }
    }

    /// `status [T]`: `ws status [T]`.
    fn status(&self) -> Plan {
        let rest =
            (self.rewritten(&[WS_STATUS], status_flags)).unwrap_or_else(|| self.rest.to_vec());
        let read = self.read(rest, &[WS_STATUS]);
        if read.has_positional()
            && let Some(plan) = self.refused(read.names_ws(), &BELOW_STATUS)
        {
            return plan;
        }
        into(WS_STATUS, read.words)
    }

    /// `restore P... [T]`: `ws discard P... [T]`.
    fn restore(&self) -> Plan {
        let read = self.read(self.rest.to_vec(), &[WS_DISCARD]);
        if !read.has_positional() {
            return Plan::Keep;
        }
        if let Some(plan) = self.refused(read.names_ws(), &BELOW_RESTORE) {
            return plan;
        }
        into(WS_DISCARD, read.words)
    }

    /// `reflog [show] NAME [T]`: `ref log NAME [T]`.
    fn reflog(&self) -> Plan {
        // 1. A leading `show` is git's default subcommand.
        let mut read = self.read(self.rest.to_vec(), &[REF_LOG]);
        if let Some((at, "show")) = read.first() {
            read = self.read(read.without_at(at), &[REF_LOG]);
        }

        // 2. A name is required; git's other subcommands name no ref.
        match read.first() {
            Some((_, word)) if !REFLOG_VERBS.contains(&word) => into(REF_LOG, read.words),
            _ => Plan::Keep,
        }
    }

    /// `worktree list [T]`: `ws list [T]`.
    fn worktree(&self) -> Plan {
        let read = self.read(self.rest.to_vec(), &[WS_LIST]);
        match read.first() {
            Some((at, "list")) => into(WS_LIST, read.without_at(at)),
            _ => Plan::Keep,
        }
    }

    /// `tag`, `tag -l`: the refs under `tags/`.
    fn tag(&self) -> Plan {
        let read = self.read(self.rest.to_vec(), &[REF_LIST]);
        if read.has_positional() {
            return Plan::Keep;
        }
        into(TAGS, read.without(&["-l", "--list"]))
    }

    /// `remote`, `remote -v`: `remote list`; `remote rm NAME`: `remote remove NAME`.
    fn remote(&self) -> Plan {
        let read = self.read(self.rest.to_vec(), &[REMOTE, REMOTE_LIST]);
        match read.first() {
            None => into(REMOTE_LIST, read.without(&["-v", "--verbose"])),
            Some((at, "rm")) => {
                let mut words = read.words;
                "remove".clone_into(&mut words[at]);
                into(REMOTE, words)
            }
            Some(_) => Plan::Keep,
        }
    }

    /// `fetch [T]`, `push [T]`: from and to `origin`.
    fn origin(&self, verb: &[&str]) -> Plan {
        let read = self.read(self.rest.to_vec(), &[verb]);
        if read.has_positional() {
            return Plan::Keep;
        }
        let path: Vec<&str> = verb.iter().copied().chain([DEFAULT_REMOTE]).collect();
        into(&path, read.words)
    }

    /// `clone URL DIR [T]`: `clone URL --into DIR [T]`.
    fn clone_repo(&self) -> Plan {
        let read = self.read(self.rest.to_vec(), &[CLONE]);
        let positionals: Vec<usize> = read.positionals().collect();
        let dashes = read.kinds.contains(&Kind::Dashes);
        let [_, dir] = positionals[..] else {
            return Plan::Keep;
        };
        if dashes || read.holds_flag(&["--into"]) {
            return Plan::Keep;
        }
        let mut words = read.words;
        words.insert(dir, "--into".to_owned());
        into(CLONE, words)
    }

    /// `log --oneline`, `log -<n>`.
    fn log(&self) -> Plan {
        match self.rewritten(&[LOG], log_flags) {
            Some(words) => into(LOG, words),
            None => Plan::Keep,
        }
    }

    /// `diff [T]` in a workspace: `ws diff [T]`, each `--path P` a trailing `P`. Its shape
    /// flags in either form.
    fn diff(&self) -> Plan {
        let rewritten = self.rewritten(&[DIFF, WS_DIFF], diff_flags);
        let changed = rewritten.is_some();
        let read = self.read(
            rewritten.unwrap_or_else(|| self.rest.to_vec()),
            &[DIFF, WS_DIFF],
        );

        // 1. A line with revisions, or outside a workspace, is the store's diff.
        if read.has_positional() || !self.inside(&read) {
            return flagged(changed, DIFF, read.words);
        }

        // 2. Paths are the workspace's, from its root.
        let named = read.names_ws();
        let (mut words, paths) = read.paths();
        if !paths.is_empty()
            && let Some(plan) = self.refused(named, &BELOW_DIFF)
        {
            return plan;
        }
        words.extend(paths);
        into(WS_DIFF, words)
    }

    /// `commit -m MSG [T] [P...]` in a workspace: `ws commit -m MSG [T] [P...]`. Its git flags
    /// in either form.
    fn commit(&self) -> Plan {
        let rewritten = self.rewritten(&[COMMIT, WS_COMMIT], commit_flags);
        let changed = rewritten.is_some();
        let read = self.read(
            rewritten.unwrap_or_else(|| self.rest.to_vec()),
            &[COMMIT, WS_COMMIT],
        );

        // 1. Only a message without the store's own flags, in a workspace, is the workspace's.
        if !read.holds_message() || read.holds_flag(&COMMIT_ONLY) || !self.inside(&read) {
            return flagged(changed, COMMIT, read.words);
        }

        // 2. Paths are the workspace's, from its root.
        if read.has_positional()
            && let Some(plan) = self.refused(read.names_ws(), &BELOW_COMMIT)
        {
            return plan;
        }
        into(WS_COMMIT, read.words)
    }

    /// `rebase [T]` in a workspace: `ws rebase [T]`.
    fn rebase(&self) -> Plan {
        let read = self.read(self.rest.to_vec(), &[REBASE, WS_REBASE]);
        if read.has_positional() || read.holds_flag(&REBASE_ONLY) || !self.inside(&read) {
            return Plan::Keep;
        }
        into(WS_REBASE, read.words)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Every place a line can be typed.
    const PLACES: [Place; 3] = [Place::Root, Place::Below, Place::Elsewhere];

    /// The git verbs of the audit, in its order, with their tiers.
    const AUDIT: [(&str, Tier); 49] = [
        ("init", Tier::One),
        ("clone", Tier::One),
        ("status", Tier::One),
        ("add", Tier::One),
        ("rm", Tier::One),
        ("mv", Tier::One),
        ("commit", Tier::One),
        ("diff", Tier::One),
        ("log", Tier::One),
        ("show", Tier::One),
        ("branch", Tier::One),
        ("switch", Tier::One),
        ("checkout", Tier::One),
        ("restore", Tier::One),
        ("merge", Tier::One),
        ("rebase", Tier::One),
        ("fetch", Tier::One),
        ("pull", Tier::One),
        ("push", Tier::One),
        ("remote", Tier::One),
        ("tag", Tier::One),
        ("stash", Tier::One),
        ("reset", Tier::One),
        ("revert", Tier::One),
        ("cherry-pick", Tier::One),
        ("blame", Tier::One),
        ("grep", Tier::One),
        ("config", Tier::One),
        ("reflog", Tier::One),
        ("rev-parse", Tier::Two),
        ("rev-list", Tier::Two),
        ("ls-files", Tier::Two),
        ("ls-tree", Tier::Two),
        ("cat-file", Tier::Two),
        ("hash-object", Tier::Two),
        ("describe", Tier::Two),
        ("for-each-ref", Tier::Two),
        ("show-ref", Tier::Two),
        ("symbolic-ref", Tier::Two),
        ("update-ref", Tier::Two),
        ("merge-base", Tier::Two),
        ("name-rev", Tier::Two),
        ("ls-remote", Tier::Two),
        ("diff-tree", Tier::Two),
        ("worktree", Tier::Two),
        ("clean", Tier::Two),
        ("archive", Tier::Two),
        ("bisect", Tier::Two),
        ("shortlog", Tier::Two),
    ];

    /// Each alias line, where it is typed, and what it expands to.
    const ALIASES: &[(&str, Place, &str)] = &[
        ("status", Place::Elsewhere, "ws status"),
        ("status -s", Place::Elsewhere, "ws status --human"),
        ("status --short --branch", Place::Root, "ws status --human"),
        ("status -sb", Place::Below, "ws status --human"),
        ("status -bs", Place::Root, "ws status --human"),
        ("status a.txt", Place::Root, "ws status a.txt"),
        ("restore a.txt b.txt", Place::Root, "ws discard a.txt b.txt"),
        ("reflog main", Place::Elsewhere, "ref log main"),
        ("reflog show main -n 3", Place::Root, "ref log main -n 3"),
        ("worktree list", Place::Elsewhere, "ws list"),
        ("tag", Place::Elsewhere, "ref list --prefix tags/"),
        ("tag -l", Place::Root, "ref list --prefix tags/"),
        ("tag --list", Place::Root, "ref list --prefix tags/"),
        ("remote", Place::Elsewhere, "remote list"),
        ("remote -v", Place::Root, "remote list"),
        ("remote --verbose", Place::Root, "remote list"),
        ("remote rm origin", Place::Root, "remote remove origin"),
        ("fetch", Place::Elsewhere, "fetch origin"),
        ("push", Place::Root, "push origin"),
        (
            "clone https://remote.invalid/x.git d",
            Place::Elsewhere,
            "clone https://remote.invalid/x.git --into d",
        ),
        ("log --oneline", Place::Elsewhere, "log --human"),
        ("log -3", Place::Root, "log -n 3"),
        (
            "diff --stat A B",
            Place::Root,
            "diff --format stat --human A B",
        ),
        (
            "diff --name-status A B",
            Place::Root,
            "diff --format names --human A B",
        ),
        (
            "diff -p A B",
            Place::Elsewhere,
            "diff --format unified --human A B",
        ),
        (
            "diff -u A B",
            Place::Root,
            "diff --format unified --human A B",
        ),
        (
            "diff --patch A B",
            Place::Root,
            "diff --format unified --human A B",
        ),
        (
            "diff -U5 A B",
            Place::Root,
            "diff --format unified --context 5 --human A B",
        ),
        (
            "diff --unified=5 A B",
            Place::Root,
            "diff --format unified --context 5 --human A B",
        ),
        ("diff", Place::Root, "ws diff"),
        ("diff --path a --path=b", Place::Root, "ws diff a b"),
        ("commit -m x", Place::Below, "ws commit -m x"),
        ("commit -am x", Place::Root, "ws commit -m x"),
        ("commit -a -m x a.txt", Place::Root, "ws commit -m x a.txt"),
        (
            "commit --all --message x",
            Place::Root,
            "ws commit --message x",
        ),
        ("rebase", Place::Below, "ws rebase"),
        ("rebase --continue", Place::Root, "ws rebase --continue"),
        // The shape flags and the order of `--human` and `--json`.
        ("status -s -b", Place::Elsewhere, "ws status --human"),
        ("status --human -s", Place::Root, "ws status --human"),
        ("status -s --json", Place::Root, "ws status --human --json"),
        ("log --oneline --human -3", Place::Root, "log --human -n 3"),
        (
            "diff --stat --human A B",
            Place::Elsewhere,
            "diff --format stat --human A B",
        ),
        ("diff --stat", Place::Root, "ws diff --format stat --human"),
        (
            "diff -U5 --path a",
            Place::Root,
            "ws diff --format unified --context 5 --human a",
        ),
    ];

    /// The flags of git's text shapes, none a flag of `diff` or `ws diff`.
    const DIFF_SHAPES: &[&str] = &[
        "--stat",
        "--name-status",
        "-p",
        "-u",
        "--patch",
        "-U",
        "--unified",
    ];

    /// Each refused line, where it is typed, and the first line of its hint.
    const HINTED: &[(&str, Place, &str)] = &[
        (
            "add a.txt",
            Place::Root,
            "git add: there is no staging area; so commit -m MSG [PATH]... in a workspace takes \
             every change, or the named paths",
        ),
        (
            "rm a.txt",
            Place::Root,
            "git rm: delete the file in the workspace; so commit -m MSG records it",
        ),
        (
            "mv a.txt b.txt",
            Place::Root,
            "git mv: move the file in the workspace; so commit -m MSG records it",
        ),
        (
            "branch",
            Place::Elsewhere,
            "git branch: so ref list lists refs; so ref set NAME REV --create makes one; so ref \
             delete NAME --expect ID removes one",
        ),
        (
            "switch main",
            Place::Root,
            "git switch: a workspace does not switch; so ws create NAME --base REV makes another",
        ),
        (
            "checkout main",
            Place::Root,
            "git checkout: so ws create NAME --base REV for another tree; so restore PATH... to \
             drop edits",
        ),
        (
            "restore",
            Place::Root,
            "git restore: name the paths, so restore PATH...; so ws discard drops every edit",
        ),
        (
            "merge main",
            Place::Root,
            "git merge: SwarmOps lands; so ws land from a workspace, or so land HEAD",
        ),
        (
            "pull",
            Place::Root,
            "git pull: so sync REMOTE --no-push brings the remote into main; so rebase in a \
             workspace follows it",
        ),
        (
            "stash",
            Place::Root,
            "git stash: so ws snapshot records the edits and leaves the files; nothing pops",
        ),
        (
            "reset --hard",
            Place::Root,
            "git reset: so ws discard [PATH]... drops edits; so op undo OP takes an operation \
             back; so ref set moves a ref",
        ),
        (
            "cherry-pick main",
            Place::Root,
            "git cherry-pick: not built; so land HEAD --base BASE lands one change's range",
        ),
        (
            "blame a.txt",
            Place::Root,
            "git blame: not built yet (M7); so history REV:PATH lists the changesets that \
             touched a path",
        ),
        (
            "grep x",
            Place::Root,
            "git grep: search the files of a workspace; so cat REV:PATH reads one at a revision",
        ),
        (
            "config user.name",
            Place::Elsewhere,
            "git config: identity is --as or SWARMOPS_AGENT (so whoami); the store is --repo or \
             SWARMOPS_REPO; remotes are so remote",
        ),
        (
            "tag v1",
            Place::Root,
            "git tag: so tag lists; so ref set tags/NAME REV --create makes one; so ref delete \
             tags/NAME --expect ID removes one",
        ),
        (
            "worktree add d",
            Place::Root,
            "git worktree: so ws create NAME [--path DIR] [--base REV]; so ws list; so ws delete \
             NAME",
        ),
        (
            "reflog",
            Place::Root,
            "git reflog: so reflog NAME lists a ref's moves; so op log every operation; nothing \
             expires",
        ),
        (
            "rev-parse main",
            Place::Root,
            "git rev-parse: so resolve REV gives the full id; so ws info a workspace's head and \
             path",
        ),
        (
            "rev-list main",
            Place::Root,
            "git rev-list: so log REV -n N; so ancestry FROM TO",
        ),
        ("ls-files", Place::Root, "git ls-files: so ls REV[:PATH]"),
        (
            "ls-tree main",
            Place::Root,
            "git ls-tree: so ls REV[:PATH] --depth N",
        ),
        (
            "cat-file -p main",
            Place::Root,
            "git cat-file: so cat REV:PATH; so blob get ID; so blob has ID",
        ),
        (
            "hash-object a.txt",
            Place::Root,
            "git hash-object: so blob put FILE",
        ),
        (
            "describe",
            Place::Root,
            "git describe: not built; so tag lists the tags",
        ),
        (
            "for-each-ref",
            Place::Root,
            "git for-each-ref: so ref list [--prefix P]",
        ),
        ("show-ref", Place::Root, "git show-ref: so ref list"),
        (
            "symbolic-ref HEAD",
            Place::Root,
            "git symbolic-ref: there are no symbolic refs; so ws info gives a workspace's head \
             and target",
        ),
        (
            "update-ref main main",
            Place::Root,
            "git update-ref: so ref set NAME REV --expect ID, or --create; so ref delete; so ref \
             txn",
        ),
        (
            "name-rev main",
            Place::Root,
            "git name-rev: not built; so ref list gives each ref's changeset",
        ),
        (
            "ls-remote",
            Place::Root,
            "git ls-remote: so fetch REMOTE, then so ref list --prefix remote/REMOTE/; so sync \
             status REMOTE",
        ),
        (
            "diff-tree main",
            Place::Root,
            "git diff-tree: so diff FROM TO --format names; so show REV",
        ),
        (
            "clean -f",
            Place::Root,
            "git clean: so ws discard PATH... drops the named paths' edits, new files included; \
             without a path, every edit",
        ),
        (
            "archive main",
            Place::Root,
            "git archive: git init --bare PATH, so git export PATH, then git archive there",
        ),
        ("bisect start", Place::Root, "git bisect: not built"),
        (
            "shortlog",
            Place::Root,
            "git shortlog: so log, counted by author_agent",
        ),
        // The bare forms, on a missing argument.
        (
            "diff",
            Place::Elsewhere,
            "in a workspace this is so ws diff; so ws create NAME makes one",
        ),
        (
            "rebase",
            Place::Elsewhere,
            "in a workspace this is so ws rebase; so ws create NAME makes one",
        ),
        (
            "commit -m x",
            Place::Elsewhere,
            "so commit -m MSG [PATH]... in a workspace commits its changes; so ws create NAME \
             makes one",
        ),
        (
            "clone https://remote.invalid/x.git",
            Place::Elsewhere,
            "name the directory: so clone URL DIR",
        ),
        // A path typed below the root.
        (
            "restore a.txt",
            Place::Below,
            "paths are read from the workspace's root, not this directory: so ws discard \
             ROOT/RELATIVE/PATH",
        ),
        (
            "commit -m x a.txt",
            Place::Below,
            "paths are read from the workspace's root, not this directory: so ws commit -m MSG \
             ROOT/RELATIVE/PATH",
        ),
        (
            "status a.txt",
            Place::Below,
            "paths are read from the workspace's root, not this directory: so ws status \
             ROOT/RELATIVE/PATH",
        ),
        (
            "diff --path a.txt",
            Place::Below,
            "paths are read from the workspace's root, not this directory: so ws diff \
             ROOT/RELATIVE/PATH",
        ),
        // One revision, in a workspace.
        (
            "diff main",
            Place::Root,
            "a path is so diff --path P; two revisions are so diff FROM TO",
        ),
    ];

    /// The hints of refused lines rather than of words.
    const SAIDS: [&Said; 9] = [
        &BELOW_RESTORE,
        &BELOW_COMMIT,
        &BELOW_STATUS,
        &BELOW_DIFF,
        &BARE_DIFF,
        &BARE_REBASE,
        &BARE_COMMIT,
        &BARE_CLONE,
        &ONE_REVISION,
    ];

    /// What a named line's placeholders are filled with; a bracketed word is dropped.
    const FILLED: &[(&str, &str)] = &[
        ("REV", "main"),
        ("NAME", "x"),
        ("MSG", "x"),
        ("DIR", "d"),
        ("PATH", "a.txt"),
        ("PATH...", "a.txt"),
        ("REV:PATH", "main:a.txt"),
        ("REV[:PATH]", "main"),
        ("URL", "https://remote.invalid/x.git"),
        ("FILE", "f"),
        ("ID", "x"),
        ("OP", "1:00000000"),
        ("N", "1"),
        ("P", "a.txt"),
        ("TEXT", "x"),
        ("HEAD", "main"),
        ("REF", "main"),
        ("REMOTE", "origin"),
        ("BASE", "main"),
        ("FROM", "main"),
        ("TO", "main"),
        ("ONTO", "main"),
        ("tags/NAME", "tags/x"),
        ("ROOT/RELATIVE/PATH", "a.txt"),
    ];

    /// The program's name, then the words of `line`, split at spaces.
    fn args(line: &str) -> Vec<OsString> {
        std::iter::once("so")
            .chain(line.split_whitespace())
            .map(OsString::from)
            .collect()
    }

    /// `line` expanded where `place` is, without the program's name.
    fn expanded(line: &str, place: Place) -> String {
        let out = expand(&args(line), &|| place);
        let words: Vec<String> = out[1..]
            .iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect();
        words.join(" ")
    }

    /// A place no rule may ask for.
    fn unasked() -> Place {
        panic!("the place was asked for a line that needs none")
    }

    /// What `parsed` is, to compare two: the operation and the grammar's reading, clap's text,
    /// or the refusal and its path.
    fn reading(parsed: &Parsed) -> String {
        match parsed {
            Parsed::Run(cli, op) => format!("run {op} {cli:?}"),
            Parsed::Text(text) => format!("text {}", text.render()),
            Parsed::Usage { error, path, .. } => {
                format!("usage {path:?} {}", error.render())
            }
        }
    }

    /// The first line of the hint `line`, typed at `place`, is refused with.
    fn hinted(line: &str, place: Place) -> Option<String> {
        let args = args(line);
        let Parsed::Usage {
            error, bin, path, ..
        } = parse(&args, &|| place)
        else {
            panic!("{line:?} at {place:?} is not refused");
        };
        hint(&error, &path, bin.as_deref(), &args, &|| place)
    }

    /// The grammar, built, so that each command holds the global flags and `--help`.
    fn grammar() -> Command {
        let mut root = Cli::command();
        root.build();
        root
    }

    /// The command `path` names in `root`.
    fn at<'a>(root: &'a Command, path: &[&str]) -> &'a Command {
        path.iter().fold(root, |cmd, name| {
            cmd.find_subcommand(name)
                .unwrap_or_else(|| panic!("no command {path:?}"))
        })
    }

    /// The long flags of the command `path` names, `--` included.
    fn longs(path: &[&str]) -> BTreeSet<String> {
        let root = grammar();
        at(&root, path)
            .get_arguments()
            .filter_map(|arg| arg.get_long())
            .map(|long| format!("--{long}"))
            .collect()
    }

    /// Whether the command `path` names takes `flag`, long (`--stat`) or short (`-s`).
    fn takes(path: &[&str], flag: &str) -> bool {
        let root = grammar();
        at(&root, path).get_arguments().any(|arg| {
            let long = arg
                .get_long()
                .is_some_and(|long| flag == format!("--{long}"));
            let short = arg
                .get_short()
                .is_some_and(|short| flag == format!("-{short}"));
            long || short
        })
    }

    /// Each leaf and group of the grammar: with its required arguments, with `--help`, with
    /// `-h`; and the program alone.
    fn grammar_lines() -> Vec<Vec<String>> {
        fn walk(cmd: &Command, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
            for sub in cmd.get_subcommands().filter(|sub| sub.get_name() != "help") {
                path.push(sub.get_name().to_owned());
                let mut required = path.clone();
                for arg in sub.get_arguments().filter(|arg| arg.is_required_set()) {
                    if let Some(long) = arg.get_long() {
                        required.push(format!("--{long}"));
                    } else if let Some(short) = arg.get_short() {
                        required.push(format!("-{short}"));
                    }
                    required.push("x".to_owned());
                }
                out.push(required);
                for help in ["--help", "-h"] {
                    let mut line = path.clone();
                    line.push(help.to_owned());
                    out.push(line);
                }
                walk(sub, path, out);
                path.pop();
            }
        }
        let mut out = vec![Vec::new(), vec!["--help".to_owned()], vec!["-h".to_owned()]];
        walk(&grammar(), &mut Vec::new(), &mut out);
        out
    }

    /// A reader of the line, as [`parse`].
    type Reader<'a> = &'a dyn Fn(&[OsString], &dyn Fn() -> Place) -> Parsed;

    /// The grammar's lines that `cli::parse` refuses with their placeholders filled, so that
    /// [`shadowed`] cannot check them; each named, so that a new one is seen. A group without
    /// its subcommand, or a leaf that needs one flag of a group, or a value other than `x`.
    const SKIPPED: &[&str] = &[
        "",
        "blob",
        "ws",
        "ws resolve",
        "ws hook x",
        "ref",
        "ref set x x",
        "ref txn --move x",
        "op",
        "git",
        "remote",
        "serve",
    ];

    /// The grammar's lines that `cli::parse` reads, at any place, and that `parse` reads
    /// otherwise.
    fn shadowed(parse: Reader<'_>) -> Vec<String> {
        let mut found = Vec::new();
        let mut skipped = Vec::new();
        let mut read = 0;
        for line in grammar_lines() {
            let args: Vec<OsString> = std::iter::once("so")
                .chain(line.iter().map(String::as_str))
                .map(OsString::from)
                .collect();
            let today = cli::parse(&args);
            if matches!(today, Parsed::Usage { .. }) {
                skipped.push(line.join(" "));
                continue;
            }
            read += 1;
            for place in PLACES {
                if reading(&parse(&args, &|| place)) != reading(&today) {
                    found.push(format!("{} at {place:?}", line.join(" ")));
                }
            }
        }
        assert!(read > 150, "only {read} of the grammar's lines parse");
        assert_eq!(skipped, SKIPPED, "the grammar's lines refused as filled");
        found
    }

    /// `line` with its placeholders filled.
    fn filled(line: &[&str]) -> Vec<OsString> {
        let mut out = vec![OsString::from("so")];
        for word in line.iter().filter(|word| !word.starts_with('[')) {
            let fill = FILLED.iter().find(|(name, _)| name == word);
            out.push(OsString::from(fill.map_or(*word, |(_, value)| value)));
        }
        out
    }

    /// The `lines` that do not run, filled, at a workspace's root.
    fn unparsed<'a>(lines: impl IntoIterator<Item = &'a [&'a str]>) -> Vec<String> {
        let place = || Place::Root;
        lines
            .into_iter()
            .filter(|line| !matches!(parse(&filled(line), &place), Parsed::Run(..)))
            .map(|line| line.join(" "))
            .collect()
    }

    #[test]
    fn the_rows_are_the_audit() {
        let rows: Vec<(&str, Tier)> = ROWS.iter().map(|row| (row.git, row.tier)).collect();
        assert_eq!(
            rows, AUDIT,
            "ROWS holds the audit's verbs, each once, in its order"
        );
    }

    #[test]
    fn every_example_does_what_its_row_says() {
        for row in ROWS {
            // 1. A row that runs names its operation; a refused one its hint.
            let runs =
                (row.classes.iter()).any(|class| matches!(class, Class::Same | Class::Alias));
            let line = row.example.join(" ");
            assert_eq!(
                row.example.first(),
                Some(&row.git),
                "{line:?} shows {}",
                row.git
            );
            assert_eq!(
                row.op.is_some(),
                runs,
                "{}: an op for a verb that runs",
                row.git
            );
            if let Some(op) = row.op {
                let parsed = parse(&args(&line), &|| Place::Root);
                assert!(
                    matches!(parsed, Parsed::Run(_, ran) if ran == op),
                    "{line:?}"
                );
            } else {
                let first = row.hint.map(|hint| render(hint, row.lines, "so"));
                assert_eq!(hinted(&line, Place::Root), first, "{line:?}");
            }

            // 2. What is left for later, and the note, belong to the row.
            let later = row.classes.contains(&Class::Later);
            assert_eq!(row.later.is_some(), later, "{}: later", row.git);
            assert!(!row.note.contains('\n'), "{}: one paragraph", row.git);
        }
    }

    #[test]
    fn every_alias_expands_to_its_target() {
        for &(line, place, target) in ALIASES {
            assert_eq!(expanded(line, place), target, "{line:?} at {place:?}");
            assert!(
                matches!(cli::parse(&args(line)), Parsed::Usage { .. }),
                "{line:?} is refused today"
            );
            assert!(
                matches!(parse(&args(line), &|| place), Parsed::Run(..)),
                "{line:?} at {place:?} runs"
            );
        }
    }

    #[test]
    fn an_expansion_only_touches_a_refused_line() {
        let found = shadowed(&parse);
        assert!(
            found.is_empty(),
            "lines read otherwise than today: {found:?}"
        );
    }

    /// The twin of [`an_expansion_only_touches_a_refused_line`]: a rule that turns `log` into
    /// `op log` before clap reads the line is caught.
    #[test]
    fn a_rule_that_shadows_a_command_is_caught() {
        let planted = |args: &[OsString], _: &dyn Fn() -> Place| {
            let mut args = args.to_vec();
            if args.get(1).is_some_and(|verb| verb == "log") {
                args.splice(1..2, ["op", "log"].map(OsString::from));
            }
            cli::parse(&args)
        };
        let found = shadowed(&planted);
        assert!(
            found.iter().any(|line| line.starts_with("log")),
            "the planted rule is caught: {found:?}"
        );
    }

    #[test]
    fn the_workspace_forms_need_a_workspace() {
        // 1. The bare forms expand in a workspace, or when the line names one.
        for line in ["diff", "commit -m x", "rebase"] {
            assert_eq!(expanded(line, Place::Elsewhere), line, "{line:?} outside");
            for place in [Place::Root, Place::Below] {
                assert_eq!(
                    expanded(line, place),
                    format!("ws {line}"),
                    "{line:?} {place:?}"
                );
            }
            let named = format!("{line} --ws w");
            assert_eq!(expanded(&named, Place::Elsewhere), format!("ws {named}"));
        }

        // 2. `commit` without a message stays `commit`'s refusal everywhere.
        for place in PLACES {
            assert_eq!(
                expanded("commit", place),
                "commit",
                "bare commit at {place:?}"
            );
        }

        // 3. A line that parses never asks for the place, nor does a rule that needs none.
        for line in [
            "diff a b",
            "commit --changes f -m x",
            "rebase main",
            "log",
            "ws status",
        ] {
            let _ = parse(&args(line), &unasked);
        }
        for line in [
            "status",
            "status -s",
            "log -3",
            "log --oneline",
            "tag",
            "fetch",
            "push",
            "remote -v",
            "remote rm x",
            "clone u d",
            "reflog main",
            "worktree list",
            "add x",
            "commit",
            "rebase main --base x",
        ] {
            let _ = expand(&args(line), &unasked);
        }
    }

    #[test]
    fn a_path_below_the_root_is_not_expanded() {
        let cases = [
            (
                "restore a.txt",
                "ws discard a.txt",
                "ws discard a.txt --ws w",
            ),
            (
                "commit -m x a.txt",
                "ws commit -m x a.txt",
                "ws commit -m x a.txt --ws w",
            ),
            ("status a.txt", "ws status a.txt", "ws status a.txt --ws w"),
            ("diff --path a.txt", "ws diff a.txt", "ws diff --ws w a.txt"),
        ];
        for (line, at_root, named) in cases {
            assert_eq!(
                expanded(line, Place::Below),
                line,
                "{line:?} below the root"
            );
            assert_eq!(expanded(line, Place::Root), at_root, "{line:?} at the root");
            let with_ws = format!("{line} --ws w");
            assert_eq!(
                expanded(&with_ws, Place::Below),
                named,
                "{with_ws:?} below the root"
            );
        }
    }

    #[test]
    fn an_alias_prints_its_targets_help() {
        for (line, target) in [
            ("status --help", "ws status --help"),
            ("tag -h", "ref list -h"),
        ] {
            let help = reading(&parse(&args(line), &|| Place::Elsewhere));
            assert!(help.starts_with("text "), "{line:?} prints help: {help}");
            assert_eq!(help, reading(&cli::parse(&args(target))), "{line:?}");
        }
    }

    #[test]
    fn an_alias_verb_is_no_command() {
        let root = grammar();
        for verb in ["status", "restore", "reflog", "worktree", "tag"] {
            assert!(
                root.find_subcommand(verb).is_none(),
                "{verb} became a command"
            );
        }
    }

    #[test]
    fn a_rewritten_flag_is_not_the_targets() {
        let cases: [(&[&str], &[&str]); 5] = [
            (WS_STATUS, &["-s", "-b", "--short", "--branch"]),
            (&["log"], &["--oneline"]),
            (&["diff"], DIFF_SHAPES),
            (WS_DIFF, DIFF_SHAPES),
            (WS_COMMIT, &["-a", "--all"]),
        ];
        for (path, flags) in cases {
            for flag in flags {
                assert!(!takes(path, flag), "{path:?} takes {flag}");
            }
        }
    }

    #[test]
    fn the_store_only_flags_are_the_grammars() {
        let lacks = |path: &[&str], target: &[&str]| -> BTreeSet<String> {
            longs(path).difference(&longs(target)).cloned().collect()
        };
        let set = |flags: &[&str]| flags.iter().map(|&flag| flag.to_owned()).collect();
        assert_eq!(
            lacks(&["commit"], WS_COMMIT),
            set(&COMMIT_ONLY),
            "commit's own flags"
        );
        assert_eq!(
            lacks(&["rebase"], WS_REBASE),
            set(&REBASE_ONLY),
            "rebase's own flags"
        );
    }

    #[test]
    fn global_flags_do_not_hide_the_verb() {
        let cases = [
            ("--repo X status", "--repo X ws status"),
            (
                "--repo=X --as a/b status -s",
                "--repo=X --as a/b ws status --human",
            ),
            ("--pretty status", "--pretty ws status"),
            ("status --repo X", "ws status --repo X"),
            ("--json status -s", "--json ws status --human"),
        ];
        for (line, target) in cases {
            assert_eq!(expanded(line, Place::Root), target, "{line:?}");
        }
    }

    #[test]
    fn protocol_lines_are_untouched() {
        for line in ["serve status", "mcp", "ws hook worktree-create"] {
            for place in PLACES {
                assert_eq!(expanded(line, place), line, "{line:?} at {place:?}");
                let today = reading(&cli::parse(&args(line)));
                assert_eq!(reading(&parse(&args(line), &|| place)), today, "{line:?}");
            }
        }
    }

    #[test]
    fn reflog_subcommands_are_not_a_ref() {
        for verb in REFLOG_VERBS {
            let line = format!("reflog {verb} x");
            assert_eq!(expanded(&line, Place::Root), line, "{line:?}");
            let first = hinted(&line, Place::Root).unwrap_or_default();
            assert!(first.starts_with("git reflog: "), "{line:?}: {first:?}");
        }
    }

    #[test]
    fn every_named_line_parses() {
        let rows = ROWS.iter().flat_map(|row| row.lines.iter().copied());
        let saids = SAIDS.iter().flat_map(|said| said.lines.iter().copied());
        let found = unparsed(rows.chain(saids));
        assert!(found.is_empty(), "named lines that do not run: {found:?}");
    }

    /// The twin of [`every_named_line_parses`]: a line with a flag no command has is caught.
    #[test]
    fn a_named_line_that_does_not_parse_is_caught() {
        let planted: &[&str] = &["ref", "set", "NAME", "REV", "--make"];
        assert_eq!(
            unparsed([planted]),
            ["ref set NAME REV --make"],
            "the planted line"
        );
    }

    #[test]
    fn a_hint_is_one_short_line() {
        let rows = ROWS
            .iter()
            .filter_map(|row| Some(render(row.hint?, row.lines, "so")));
        let saids = SAIDS.iter().map(|said| render(said.text, said.lines, "so"));
        for line in rows.chain(saids) {
            assert!(!line.is_empty(), "an empty hint");
            assert!(line.len() <= 120, "{} bytes: {line:?}", line.len());
            assert!(
                !line.contains(['\n', '{', '}']),
                "one line, all filled: {line:?}"
            );
        }
    }

    #[test]
    fn every_refused_row_has_a_hint() {
        let root = grammar();
        let refused = [Class::Different, Class::NoConcept, Class::Later];
        for row in ROWS {
            let command = root.find_subcommand(row.git).is_some();
            let hinted = row.classes.iter().any(|class| refused.contains(class));
            if hinted && !command {
                assert!(row.hint.is_some(), "{} has no hint", row.git);
            }
            if command {
                assert!(
                    row.hint.is_none(),
                    "{} is a command: its hint is never shown",
                    row.git
                );
            }
        }
        let count = ROWS.iter().filter(|row| row.hint.is_some()).count();
        assert_eq!(count, 36, "the hinted words");
    }

    #[test]
    fn every_refused_line_gets_its_line() {
        assert_eq!(
            HINTED.len(),
            45,
            "36 words, 4 bare forms, 4 lines below, 1 revision"
        );
        for &(line, place, first) in HINTED {
            assert_eq!(
                hinted(line, place).as_deref(),
                Some(first),
                "{line:?} at {place:?}"
            );
        }
    }

    /// The twin of [`every_refused_line_gets_its_line`]: a refusal with no row gets no line.
    #[test]
    fn a_word_without_a_row_gets_no_line() {
        let cases = [
            ("frobnicate", Place::Root),
            ("am", Place::Root),
            ("diff main", Place::Elsewhere),
            ("commit -m x --parent main", Place::Root),
        ];
        for (line, place) in cases {
            assert_eq!(hinted(line, place), None, "{line:?} at {place:?}");
        }
    }

    #[test]
    fn a_nested_unknown_word_gets_no_line() {
        for line in ["ws add x", "remote config", "git archive"] {
            assert_eq!(hinted(line, Place::Root), None, "{line:?}");
        }
    }

    #[test]
    fn a_hint_names_the_program_it_was_run_as() {
        let args = args("add x");
        let Parsed::Usage { error, path, .. } = cli::parse(&args) else {
            panic!("add is refused");
        };
        for (bin, name) in [
            (Some("so"), "so"),
            (Some("swarmops"), "swarmops"),
            (None, "swarmops"),
        ] {
            let first = hint(&error, &path, bin, &args, &|| Place::Root).unwrap_or_default();
            let named = format!("area; {name} commit -m MSG");
            assert!(first.contains(&named), "{bin:?}: {first:?}");
        }
    }
}
