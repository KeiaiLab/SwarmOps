//! The command line (operations spec §1): global flags, one subcommand per operation, and the
//! operation's name each subcommand stands for in the envelope.

use std::ffi::OsString;
use std::path::Path;

use clap::error::ErrorKind;
use clap::{
    ArgGroup, ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
};
use swarmops_api::remote::{DEFAULT_REMOTE, DEFAULT_SYNC_STATUS_LIMIT};
use swarmops_api::{
    DEFAULT_DIFF_CONTEXT, DEFAULT_DIFF_LIMIT, DEFAULT_DIFF_MAX_BYTES, DEFAULT_LOG_LIMIT,
    DEFAULT_MAX_BYTES, DEFAULT_MAX_CHANGES, DEFAULT_MAX_CHANGESETS, DEFAULT_OP_LOG_LIMIT,
    DEFAULT_PATH_HISTORY_LIMIT, DEFAULT_READ_TREE_LIMIT, LineRange, LossyClass,
};
use swarmops_api::{DEFAULT_MAX_ATTEMPTS, DEFAULT_MAX_CONFLICT_BYTES};

use crate::output::printable;

/// The envelope's `op` for a command line that names no operation.
pub const NO_OP: &str = "swarmops";

// The help of `--expect`, as `help` (`-h`) and `long_help` (`--help`) strings rather than doc
// comments, which rustdoc would read `<actual>` in as HTML.

/// `--expect`'s short help.
const EXPECT_HELP: &str = "The changeset the ref holds now: a full id, not a ref name";

/// `ref set --expect`'s long help.
const SET_EXPECT_LONG_HELP: &str = "The changeset the ref holds now.

Give the full changeset id you read (from ref list, show, log or a commit's result). A ref \
name or a relative revision such as main~1 is read from the store when the command runs \
instead of pinning the id you read: the ref's own name always matches (a deleted ref errors \
instead), so the compare-and-swap protects nothing, and main~1 on main never matches. An id \
prefix pins the id you read only until another changeset shares it; then it is E_AMBIGUOUS_ID.

After E_CAS_MISMATCH, land your change instead (swarmops land merges it onto the ref's head), or \
re-apply your edits to the content at the error's `actual`, commit with --parent <actual>, then \
retry with --expect <actual>. Do not re-send your old whole-file puts: they would revert the \
winner's edits to shared files.";

/// `ref delete --expect`'s long help.
const DELETE_EXPECT_LONG_HELP: &str = "The changeset the ref holds now.

Give the full changeset id you read (from ref list, show, log or a commit's result). A ref \
name or a relative revision such as main~1 is read from the store when the command runs \
instead of pinning the id you read: the ref's own name always matches (a deleted ref errors \
instead), so the compare-and-swap protects nothing, and main~1 on main never matches. An id \
prefix pins the id you read only until another changeset shares it; then it is E_AMBIGUOUS_ID.

After E_CAS_MISMATCH, the ref holds the error's `actual`, which someone else moved it to: look \
at it (swarmops show <actual>) before you delete it with --expect <actual>.";

/// `commit --advance`'s short help.
const ADVANCE_HELP: &str =
    "Also move REF to the new changeset, in the same transaction, if REF still names --parent";

/// `commit --advance`'s long help.
const ADVANCE_LONG_HELP: &str = "Also move REF (main or release/<name>) to the new changeset, in \
the same transaction: both are recorded, or neither.

The move happens only if REF still names --parent (for a root commit, only if REF does not exist \
yet), so it is always a one-step fast-forward. Give --parent as the full changeset id you read \
REF at (from ref list, show, log or a commit's result): a ref name resolves when the command \
runs, and without --parent the commit builds on REF's head as it is then, so the move catches \
only a race from that moment on.

After E_CAS_MISMATCH nothing was recorded, neither the changeset nor the move: commit your \
change without --advance and land it (swarmops land merges it onto REF's head), or re-apply your \
edits to the content at the error's `actual` and commit with --parent <actual> --advance REF. Do \
not re-send your old whole-file puts: they would revert the winner's edits to shared files.";

/// `git import`'s short help.
const GIT_IMPORT_HELP: &str = "Bring a git repository's history in: every commit --ref reaches \
that the store does not hold, then move --into to it";

/// `git import`'s long help.
const GIT_IMPORT_LONG_HELP: &str = "Bring a git repository's history in: every commit --ref \
reaches that the store does not hold becomes a changeset, parents first, and --into moves to the \
changeset of --ref's commit, created if it does not exist.

The repository is read on this host, at PATH, relative to the working directory: a work tree, \
which holds .git, or a bare repository. Each changeset keeps its git commit's id in its git.oid \
metadata, and its author and committer in its signatures; it records no agent. Run again, it takes \
only the commits after the one --into's head holds, and writes nothing when that is --ref's \
commit. A head that holds no git commit, or one --ref does not reach, fails with \
E_GIT_IMPORT_DIVERGED (exit 3), and nothing is written.

Content a changeset cannot hold as git has it is refused by class (E_GIT_IMPORT_LOSSY, naming \
the commit, the path and the class) unless --lossy allows that class: then it is dropped or \
normalized, and the changeset names the class in its git.lossy metadata.";

/// `git import --ref`'s help.
const GIT_REF_HELP: &str =
    "The commit to import, with every commit it reaches: HEAD, a ref's name or a commit id";

/// `git import --into`'s help.
const GIT_INTO_HELP: &str = "The ref to move to that commit's changeset: main or release/<name>";

/// `git import --lossy`'s help.
const GIT_LOSSY_HELP: &str =
    "A class of content to drop or normalize instead of refusing it, or all; repeatable";

/// `git import --gitlinks`'s help.
const GIT_GITLINKS_HELP: &str = "What to do with a submodule's entry: refuse it, or drop it as \
--lossy gitlinks does; given, it decides that class whatever --lossy says";

/// `git export`'s short help.
const GIT_EXPORT_HELP: &str = "Send the store's history to git: write every commit of --ref's \
history that --branch's last export does not reach into the git repository at PATH, then move \
--branch to it";

/// `git export`'s long help.
const GIT_EXPORT_LONG_HELP: &str = "Send the store's history to git: every commit of --ref's \
history that the last export into --branch does not reach is written into the git repository at \
PATH, parents first, made durable, and --branch moves to the export of --ref's head, created if it \
does not exist.

The repository is written on this host, at PATH, relative to the working directory: a bare \
repository (git init --bare makes one), or a work tree, which holds .git and whose checked-out \
branch is refused (E_INVALID_ARG). A changeset imported from git exports to the commit it came from, byte for byte; \
one made in the store exports to a commit with its own signatures and SwarmOps-* trailers, the \
same commit every time. Run again, it writes only what the branch does not reach, and nothing \
when the branch is at the export of --ref's head. It needs no identity.

A branch at a commit that is no export of --ref's head or of an ancestor of it fails with \
E_GIT_EXPORT_DIVERGED (exit 3), details.reason saying why, and nothing is written; --force moves \
it all the same, still by compare-and-swap. Then git push sends the branch on.";

/// `git export --ref`'s help.
const GIT_EXPORT_REF_HELP: &str = "The store ref whose history to export: main or release/<name>";

/// `git export --branch`'s help.
const GIT_EXPORT_BRANCH_HELP: &str = "The git branch to write, refs/heads/<BRANCH>: main, or a trunk named otherwise, such as stable";

/// `git export --force`'s help.
const GIT_EXPORT_FORCE_HELP: &str = "Move a branch that diverged all the same, replacing what git \
holds there (by compare-and-swap; never a checked-out branch)";

/// `commit --key`'s short help.
const KEY_HELP: &str =
    "A uuid v7 that makes a retry safe: the same commit run again replays (replayed: true)";

/// `commit --key`'s long help.
const KEY_LONG_HELP: &str = "A uuid v7 (RFC 9562) that makes a retry of this commit safe: make a \
new one for each commit, and give it again only to retry that commit.

The commit signs at the key's time, to the second, and records the key as its commit_key \
metadata, so the same command run again on the same parent makes the same changeset, which the \
store holds already: the result says replayed: true, with the first run's changeset, and \
nothing is written. A commit whose parent is the changeset its key made (without --parent, \
main's head, or REF's with --advance) replays that changeset instead of committing it again on \
top of itself.

Give --parent as the full changeset id you read: without it the commit builds on main's head \
(REF's, with --advance) as it is when the command runs, so a retry after that head moved to \
another changeset commits again, on the new head.";

/// `land --target`'s help.
const TARGET_HELP: &str = "The ref to land on: main or release/<name>";

/// `land --key`'s short help.
const LAND_KEY_HELP: &str =
    "Makes a retry safe: the same land run again replays it (status: replayed)";

/// `land --key`'s long help.
const LAND_KEY_LONG_HELP: &str = "Makes a retry of this land safe: 1 to 64 letters, digits and \
'-', such as a uuid v7. Make a new one for each change you land, and give it again only to retry \
that land.

A land with a key first looks down the ref's first parents, to the change's base, for a \
changeset landed with the same key: one squashed from this HEAD is the land made before, which \
the land replays (status: replayed) without writing anything; one from another head is \
E_INVALID_ARG. Without a key, the same land run again finds its change on the ref already: \
E_NOTHING_TO_LAND.";

/// `land --resolve`'s short help.
const RESOLVE_HELP: &str =
    "Resolve the conflicts a report named: resolutions as JSON in FILE, or - for stdin";

/// `land --resolve`'s long help.
const RESOLVE_LONG_HELP: &str = "Resolve the conflicts an E_CONFLICT report named, without a \
checkout: resolutions as a JSON object in FILE, or - for stdin, at most 64 MiB (swarmops schema \
ResolutionsInput).

{\"against\": THEIRS, \"files\": {PATH: RESOLUTION, ...}}: THEIRS is the report's theirs, by its \
full id, and each RESOLUTION one of {\"take\": \"ours\"}, {\"take\": \"theirs\"}, {\"content\": \
CONTENT} with an optional \"mode\" (CONTENT as a commit's change gives it: inline, base64, a \
file, or a stored blob by id) and {\"delete\": true}. A resolution decides its path and every \
path below it; the rest of the merge stands.

A resolution applies while the ref holds its path as THEIRS did, whether the path conflicts or \
not. A path the ref changed since is stale: the land fails with E_CONFLICT again, its report \
naming the path in stale, as it does for each conflict left unresolved. Read the report's \
theirs, and resolve against it.";

/// `clone`'s short help.
const CLONE_HELP: &str = "Make a store from a git remote: fetch it, import its default branch \
into main, and record it as a remote";

/// `clone`'s long help.
const CLONE_LONG_HELP: &str = "Make a store from a git remote: a new store in --into, the remote \
recorded as --remote (origin), fetched, and its followed branch (--branch, else the remote's \
default branch) imported into main.

The URL is an https or ssh URL, or the scp form user@host:path; a plain http URL only to a \
loopback address, and a URL holding a password is refused (leave the token to git's credential \
helper, or name its variable with --token-env). Git 2.39 or later runs the transfer, with the \
user's own credentials, proxies and ssh configuration. It needs an identity, as git import does: it moves main. Run again into the \
same directory with the same URL, it continues an interrupted clone.";

/// `push`'s target's help.
const PUSH_TARGET_HELP: &str = "What to send, as <ref>[:<branch>]: the store ref (main, the \
default, or release/<name>) and the remote branch (by default the remote's followed branch for \
main, the same name for release/<name>)";

/// `push`'s long help.
const PUSH_LONG_HELP: &str = "Send a store ref to a remote branch by compare-and-swap, never by \
force: the remote moves only from the head the last fetch saw, to a descendant of it.

The ref is exported into the store's mirror on top of what the last fetch saw of the branch, so \
the export only appends, then git pushes it with a lease on that value. A remote that moved since \
fails with E_REMOTE_DIVERGED (exit 3) and nothing is sent: fetch, or sync. Run again, it sends \
nothing and starts no git. It needs no identity: a push is an export.";

/// `sync`'s long help.
const SYNC_LONG_HELP: &str = "Fetch the remote, land main's unpublished changes onto its head, \
move main there, and push it.

Changes the remote holds already (a SwarmOps-Changeset, SwarmOps-Squashed-From or \
SwarmOps-Land-Key line in one of its commits, or a change that merges as none) are dropped; the \
rest land onto the remote's head one by one, as land merges them. A remote that moved meanwhile \
makes it fetch and land again, up to --max-attempts. With --no-push it stops before the push, for \
a branch that takes pull requests only. It needs an identity: it moves main. sync status shows \
where main and the remote stand, offline.";

/// Source control for coding-agent swarms. Every command but mcp prints one JSON envelope
/// on stdout.
#[derive(Parser, Debug)]
#[command(name = "swarmops", version)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,
    #[command(subcommand)]
    pub command: Command,
}

/// The flags every command takes, before or after its subcommand.
#[derive(Args, Debug, Default, Clone)]
pub struct Global {
    /// The store to use (default: $SWARMOPS_REPO).
    #[arg(long, global = true, value_name = "STORE")]
    pub repo: Option<String>,
    // Help given as `help` rather than a doc comment: rustdoc would take `<family>` for HTML.
    #[arg(
        long = "as",
        global = true,
        value_name = "AGENT",
        help = "The agent to act as, <family>/<instance> (default: $SWARMOPS_AGENT)"
    )]
    pub agent: Option<String>,
    /// Print the JSON envelope: the default, unless $SWARMOPS_FORMAT is human.
    #[arg(long, global = true, overrides_with = "human")]
    pub json: bool,
    /// Print a short text rendering instead of JSON.
    #[arg(long, global = true, overrides_with = "json")]
    pub human: bool,
    /// Pretty-print the JSON (default: when stdout is a terminal).
    #[arg(
        long,
        global = true,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true"
    )]
    pub pretty: Option<bool>,
}

/// One operation.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a store in DIR, and describe it.
    Init {
        /// The directory to create the store in.
        dir: String,
        /// The repository's name, at most 255 bytes (default: the directory's name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Describe the store.
    Info,
    /// Show the identity this caller's changes carry.
    ///
    /// An identified caller gets its agent, its run id when it has one, and auth "trust"; a
    /// caller without an identity gets auth "anonymous" and no agent field at all.
    Whoami,
    /// Print the JSON Schema of every request and response type, or of TYPE's.
    Schema {
        /// One type's name, such as CommitReq.
        #[arg(value_name = "TYPE")]
        name: Option<String>,
    },
    /// Record a changeset from a list of changes; with --advance, also move that ref to it.
    Commit {
        /// The commit message.
        #[arg(short, long)]
        message: String,
        /// A JSON file holding an array of changes, or - for stdin: at most 64 MiB.
        #[arg(long, value_name = "FILE")]
        changes: String,
        /// The changeset to commit on top of (default: main's head, if main exists).
        #[arg(long, value_name = "REV")]
        parent: Option<String>,
        /// Record the changeset even if it changes nothing.
        #[arg(long)]
        allow_empty: bool,
        #[arg(long, value_name = "REF", help = ADVANCE_HELP, long_help = ADVANCE_LONG_HELP)]
        advance: Option<String>,
        #[arg(long, value_name = "UUID", help = KEY_HELP, long_help = KEY_LONG_HELP)]
        key: Option<String>,
    },
    /// Merge a change three-way onto a ref and publish it there as one new changeset.
    ///
    /// The change is HEAD with its first-parent chain since the base, squashed: the new
    /// changeset's parent is the ref's head, it holds what the chain changed merged with what the
    /// ref gained since the base, and the ref moves to it in the same transaction. A ref that
    /// moves meanwhile makes the land merge again onto its new head and retry, up to
    /// --max-attempts times. A conflict is E_CONFLICT (exit 3), with each conflicting file in
    /// details.report, and nothing is recorded; land again with --resolve to resolve it without
    /// a checkout.
    Land {
        /// The changeset to land: the change's head, such as the changeset a commit made.
        #[arg(value_name = "HEAD")]
        head: String,
        /// Where the change starts: a first-parent ancestor of HEAD, whose own changes are not
        /// landed (default: the merge base of HEAD and the ref's head, along first parents).
        #[arg(long, value_name = "REV")]
        base: Option<String>,
        #[arg(long, value_name = "REF", default_value = "main", help = TARGET_HELP)]
        target: String,
        /// The new changeset's message (default: HEAD's).
        #[arg(short, long, value_name = "MSG")]
        message: Option<String>,
        #[arg(long, value_name = "KEY", help = LAND_KEY_HELP, long_help = LAND_KEY_LONG_HELP)]
        key: Option<String>,
        /// How many times to try to publish before E_CAS_RETRY_EXHAUSTED: 1 to 32.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_MAX_ATTEMPTS)]
        max_attempts: u32,
        /// The most bytes of hunk text an E_CONFLICT report carries; past them it leaves the
        /// texts out (truncated) and keeps every line position and blob id.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_MAX_CONFLICT_BYTES)]
        max_conflict_bytes: u64,
        #[arg(long, value_name = "FILE", help = RESOLVE_HELP, long_help = RESOLVE_LONG_HELP)]
        resolve: Option<String>,
    },
    /// Store content as a blob for a commit to name by its id, or ask which blobs the store
    /// holds.
    #[command(subcommand)]
    Blob(BlobCommand),
    /// Name the changeset REV names, and whether REV is a ref's name.
    Resolve {
        /// The revision: a ref name, a full changeset id or a unique prefix of 12 or more of its
        /// hex digits, then any number of ~N suffixes.
        rev: String,
    },
    /// Print one file at a revision, or part of it: from a byte, or a range of lines.
    Cat {
        // Split at the first ':' by commands::split_target: M1's revision grammar has no ':'.
        // `ws:<name>` and `cr:CR-n[@k]` arrive later, and the split must become prefix-aware.
        /// The revision and the file's path, split at the first ':'.
        #[arg(value_name = "REV:PATH")]
        target: String,
        /// Start at byte N of the file; a truncated result's next_offset reads on from there.
        #[arg(long, value_name = "N", default_value_t = 0, conflicts_with = "lines")]
        offset: u64,
        /// Print lines A to B only, numbered from 1, both included, such as 10-20.
        #[arg(long, value_name = "A-B", value_parser = line_range)]
        lines: Option<LineRange>,
        /// The most content bytes to print.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_MAX_BYTES)]
        max_bytes: u64,
    },
    /// List one directory at a revision (the root without a PATH), and with --depth the
    /// directories below it.
    Ls {
        // Split at the first ':' as `cat`'s target is (see there).
        /// The revision, and the directory's path after the first ':'.
        #[arg(value_name = "REV[:PATH]")]
        target: String,
        /// How many levels to list: 1 for the directory's own entries, 2 for theirs too, and so
        /// on, up to 64.
        #[arg(long, value_name = "N", default_value_t = 1)]
        depth: u32,
        /// The most entries to list.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_READ_TREE_LIMIT)]
        limit: usize,
        /// Resume a listing: the previous page's cursor, exactly as it was returned.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Show one changeset.
    Show {
        /// The changeset.
        rev: String,
        /// The most changes to list.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_MAX_CHANGES)]
        max_changes: usize,
    },
    /// List history along first parents, newest first.
    Log {
        /// Where to start (default: main).
        rev: Option<String>,
        /// The most changesets to list.
        #[arg(short = 'n', long, value_name = "N", default_value_t = DEFAULT_LOG_LIMIT)]
        limit: usize,
        /// Resume a log: the previous page's cursor, exactly as it was returned.
        #[arg(long)]
        cursor: Option<String>,
        /// Only the changesets that change PATH, or anything below it; give it again for more.
        #[arg(long = "path", value_name = "PATH")]
        paths: Vec<String>,
    },
    /// List the changesets that changed a path, newest first, along first parents.
    History {
        // Split at the first ':' as `cat`'s target is (see there).
        /// The revision and the path (a file, or a directory), split at the first ':'.
        #[arg(value_name = "REV:PATH")]
        target: String,
        /// The most changesets to list.
        #[arg(short = 'n', long, value_name = "N", default_value_t = DEFAULT_PATH_HISTORY_LIMIT)]
        limit: usize,
        /// Resume a history: the previous page's cursor, exactly as it was returned.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// List the files that differ between two revisions, a page at a time.
    Diff(DiffArgs),
    /// List refs, or move one by compare-and-swap.
    #[command(subcommand)]
    Ref(RefCommand),
    /// List the store's operations, or undo one.
    #[command(subcommand)]
    Op(OpCommand),
    /// Check the store's consistency.
    Verify {
        /// Re-hash every object, and check the derived values against the objects, too.
        #[arg(long)]
        deep: bool,
    },
    /// Bring history in from git, or send it back.
    #[command(subcommand)]
    Git(GitCommand),
    /// Make a store from a git remote.
    #[command(about = CLONE_HELP, long_about = CLONE_LONG_HELP)]
    Clone {
        /// The remote's URL.
        url: String,
        /// The directory to make the store in: absent or empty.
        #[arg(long, value_name = "DIR")]
        into: String,
        /// The repository's name (default: the URL's last path component, without .git).
        #[arg(long)]
        name: Option<String>,
        /// The remote branch main follows (default: the remote's default branch).
        #[arg(long)]
        branch: Option<String>,
        /// The name to record the remote under.
        #[arg(long, value_name = "NAME", default_value = DEFAULT_REMOTE)]
        remote: String,
        /// The environment variable that holds a token for the remote, by its name.
        #[arg(long, value_name = "VAR")]
        token_env: Option<String>,
        /// A class of content the import may drop or normalize, as git import's.
        #[arg(long, value_name = "CLASS")]
        lossy: Vec<LossyArg>,
    },
    /// Add, remove or list the store's git remotes.
    #[command(subcommand)]
    Remote(RemoteCommand),
    /// Fetch a git remote into the store's mirror, and its followed branch into the store.
    ///
    /// Git 2.39 or later fetches the remote's branches and tags into the store's mirror; the
    /// followed branch's commits become changesets, with no ref moved: main never moves. Run
    /// again with nothing new on the remote, it imports nothing. It needs no identity.
    Fetch {
        /// The remote, by its name.
        remote: String,
    },
    /// Send a store ref to a git remote's branch by compare-and-swap, never by force.
    #[command(long_about = PUSH_LONG_HELP)]
    Push {
        /// The remote, by its name.
        remote: String,
        #[arg(value_name = "REF[:BRANCH]", help = PUSH_TARGET_HELP)]
        target: Option<String>,
    },
    /// Land main's unpublished changes onto a git remote's head, and push them.
    #[command(
        long_about = SYNC_LONG_HELP,
        args_conflicts_with_subcommands = true,
        subcommand_negates_reqs = true
    )]
    Sync {
        /// The remote, by its name.
        #[arg(required = true)]
        remote: Option<String>,
        /// Land onto the remote's head and move main, but push nothing.
        #[arg(long)]
        no_push: bool,
        /// How many times to fetch and land again when the remote or main moved meanwhile:
        /// 1 to 32.
        #[arg(
            long,
            value_name = "N",
            default_value_t = DEFAULT_MAX_ATTEMPTS,
            value_parser = clap::value_parser!(u32).range(1..=32)
        )]
        max_attempts: u32,
        /// `sync status`.
        #[command(subcommand)]
        status: Option<SyncCommand>,
    },
    /// Serve the store's operations to an agent as an MCP server on stdin and stdout.
    ///
    /// One tool per operation (repo_info, resolve_rev, read_file, read_tree, show, log,
    /// path_history, diff, op_log, ref_list, ref_set, ref_delete, undo, commit, land, blob_put,
    /// blob_has, verify, and the git remotes' remote_list, fetch, push, sync and sync_status),
    /// each taking the operation's request as its arguments and returning the envelope the other
    /// commands print. The store and the identity are the server's, --repo and
    /// --as or their variables, never a call's. Speaks MCP 2026-07-28, and 2025-11-25 and
    /// 2025-06-18 to clients that open with initialize. Prints no envelope: stdout carries the
    /// protocol until stdin ends, and a command line it refuses has its envelope on stderr. The
    /// derive command has no tool: it is maintenance, no agent's work; nor have clone and remote
    /// add|remove, so that an agent cannot point the store at a URL of its choosing.
    Mcp,
    /// Derive the eager derived values of new changesets, ahead of any reader.
    ///
    /// Once, it derives the values of the changesets the journal records in up to 1,000 frames,
    /// from its first frame or after --after N, and gives the last frame it read as next: run it
    /// with --after next to go on. With --follow it runs until it is killed: from the journal's
    /// end (from its first frame with --from-start, or after --after N) it derives the changesets
    /// as they are recorded, looking again every --interval-ms once it has caught up, and prints
    /// one envelope a line for each run that derived something. A run that fails ends it with
    /// that run's envelope and exit code. Commits derive nothing.
    Derive {
        /// Go on after journal frame N, a previous run's next (default: the first frame; with
        /// --follow, the journal's end).
        #[arg(long, value_name = "N", conflicts_with = "from_start")]
        after: Option<u64>,
        /// Keep deriving what the journal records, until killed.
        #[arg(long)]
        follow: bool,
        /// With --follow: how many milliseconds to wait before looking again, once caught up.
        #[arg(
            long,
            value_name = "N",
            default_value_t = 500,
            requires = "follow",
            value_parser = clap::value_parser!(u64).range(1..)
        )]
        interval_ms: u64,
        /// With --follow: start from the journal's first frame, not its end.
        #[arg(long, requires = "follow")]
        from_start: bool,
    },
}

/// `blob`'s operations.
#[derive(Subcommand, Debug)]
pub enum BlobCommand {
    /// Store FILE's content as a blob, outside any commit, and print its id, which a change
    /// names it by: {"blob": ID}. A change list holds at most 64 MiB and a commit's contents
    /// 1 GiB together; a blob named by id counts toward neither.
    Put {
        /// The file whose content to store, or - for stdin.
        #[arg(value_name = "FILE")]
        file: String,
    },
    /// Of the IDs given, list those the store holds blobs under.
    Has {
        /// Blob ids, 64 hex digits each.
        #[arg(value_name = "ID", required = true)]
        ids: Vec<String>,
    },
}

/// `diff`'s arguments.
#[derive(Args, Debug)]
pub struct DiffArgs {
    /// The revision to diff from.
    pub from: String,
    /// The revision to diff to.
    pub to: String,
    /// Only the files at or below this path; give it again for more.
    #[arg(long = "path", value_name = "P")]
    pub paths: Vec<String>,
    /// What to say of each file.
    #[arg(long, value_enum, default_value_t = DiffFormatArg::Names)]
    pub format: DiffFormatArg,
    /// The unchanged lines around each change in a unified hunk.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_DIFF_CONTEXT)]
    pub context: u32,
    /// The most files to list.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_DIFF_LIMIT)]
    pub limit: usize,
    /// Resume a diff: the previous page's cursor, exactly as it was returned.
    #[arg(long)]
    pub cursor: Option<String>,
    /// The most bytes of a unified page's files, as JSON (the first file comes all the same,
    /// its hunks cut to fit).
    #[arg(long, value_name = "N", default_value_t = DEFAULT_DIFF_MAX_BYTES)]
    pub max_bytes: u64,
}

/// What `diff` says of each file (`--format`).
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffFormatArg {
    /// Its path and status.
    Names,
    /// Also whether it is binary and, for text, the lines it inserts and deletes.
    Stat,
    /// Also its kinds, its blobs and, for text, its hunks.
    Unified,
}

/// `ref`'s operations.
#[derive(Subcommand, Debug)]
pub enum RefCommand {
    /// List every ref.
    List,
    /// Point ref NAME at REV: move it from --expect, or --create it.
    #[command(group(ArgGroup::new("from").required(true).args(["expect", "create"])))]
    Set {
        #[arg(help = "main or release/<name>")]
        name: String,
        /// The revision to point it at.
        rev: String,
        #[arg(long, value_name = "REV", help = EXPECT_HELP, long_help = SET_EXPECT_LONG_HELP)]
        expect: Option<String>,
        /// Create the ref; if it holds REV already that is a replay (already_applied), at
        /// another target E_CAS_MISMATCH.
        #[arg(long)]
        create: bool,
    },
    /// Delete ref NAME.
    Delete {
        /// The ref.
        name: String,
        #[arg(long, value_name = "REV", help = EXPECT_HELP, long_help = DELETE_EXPECT_LONG_HELP)]
        expect: String,
    },
}

/// `git`'s operations.
#[derive(Subcommand, Debug)]
pub enum GitCommand {
    // Help given as `about` and the args' `help` rather than doc comments: rustdoc would take
    // `<name>` for HTML.
    #[command(about = GIT_IMPORT_HELP, long_about = GIT_IMPORT_LONG_HELP)]
    Import {
        /// The git repository's directory.
        path: String,
        #[arg(long = "ref", value_name = "GIT_REF", default_value = "HEAD", help = GIT_REF_HELP)]
        git_ref: String,
        #[arg(long, value_name = "REF", default_value = "main", help = GIT_INTO_HELP)]
        into: String,
        #[arg(long, value_name = "CLASS", help = GIT_LOSSY_HELP)]
        lossy: Vec<LossyArg>,
        #[arg(long, value_name = "WHAT", help = GIT_GITLINKS_HELP)]
        gitlinks: Option<Gitlinks>,
    },
    #[command(about = GIT_EXPORT_HELP, long_about = GIT_EXPORT_LONG_HELP)]
    Export {
        /// The git repository's directory.
        path: String,
        #[arg(long = "ref", value_name = "REF", default_value = "main", help = GIT_EXPORT_REF_HELP)]
        source_ref: String,
        #[arg(
            long,
            value_name = "BRANCH",
            default_value = "main",
            help = GIT_EXPORT_BRANCH_HELP
        )]
        branch: String,
        #[arg(long, help = GIT_EXPORT_FORCE_HELP)]
        force: bool,
    },
}

/// `remote`'s operations.
#[derive(Subcommand, Debug)]
pub enum RemoteCommand {
    /// Record a git remote in the store.
    ///
    /// The name is 1 to 63 lowercase letters, digits, dots, underscores and dashes, starting with
    /// a letter or a digit, not ending in a dot, and not status. Without --branch the remote is
    /// asked for its default branch (a network call); an empty remote gives main, with a
    /// warning. No credential is stored: --token-env records a variable's name. It needs no
    /// identity.
    Add {
        /// The remote's name.
        name: String,
        /// The remote's URL: an https or ssh URL, or the scp form user@host:path.
        url: String,
        /// The remote branch main follows (default: the remote's default branch).
        #[arg(long)]
        branch: Option<String>,
        /// The environment variable that holds a token for the remote, by its name.
        #[arg(long, value_name = "VAR")]
        token_env: Option<String>,
        /// A class of content the import of the remote's commits may drop or normalize.
        #[arg(long, value_name = "CLASS")]
        lossy: Vec<LossyArg>,
    },
    /// Forget a git remote, and the mirror's refs of it; no changeset is removed.
    Remove {
        /// The remote's name.
        name: String,
    },
    /// List the store's git remotes.
    List,
}

/// `sync`'s operation besides syncing.
#[derive(Subcommand, Debug)]
pub enum SyncCommand {
    /// Show how main and a git remote's followed branch stand, as the last fetch saw it.
    ///
    /// Offline: it reads the store and its mirror only, takes no lock and runs no git. A branch
    /// never fetched gives fetched false.
    Status {
        /// The remote, by its name.
        remote: String,
        /// The most unpublished changesets to list.
        #[arg(short = 'n', long, value_name = "N", default_value_t = DEFAULT_SYNC_STATUS_LIMIT)]
        limit: usize,
    },
}

/// A class of git content `git import --lossy` allows, or all of them (`swarmops_api::LossyClass`
/// gives each class's meaning).
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[value(rename_all = "snake_case")]
pub enum LossyArg {
    /// A submodule's entry: dropped.
    Gitlinks,
    /// A name that is not UTF-8, or is . or ..: dropped.
    Paths,
    /// A mode git does not write, such as 100664: normalized.
    Modes,
    /// A tree out of git's order, or naming an entry twice: put in order.
    TreeOrder,
    /// An entry naming an empty tree: dropped.
    EmptyTrees,
    /// A commit whose bytes the changeset cannot keep: kept as its fields hold it.
    Nonstandard,
    /// Every class.
    All,
}

/// What `git import --gitlinks` does with a submodule's entry.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gitlinks {
    /// Refuse the commit that holds one (E_GIT_IMPORT_LOSSY).
    Refuse,
    /// Drop it, and name the class in the changeset's git.lossy metadata.
    Drop,
}

impl LossyArg {
    /// The class it names; `None` for `all`.
    fn class(self) -> Option<LossyClass> {
        Some(match self {
            LossyArg::Gitlinks => LossyClass::Gitlinks,
            LossyArg::Paths => LossyClass::Paths,
            LossyArg::Modes => LossyClass::Modes,
            LossyArg::TreeOrder => LossyClass::TreeOrder,
            LossyArg::EmptyTrees => LossyClass::EmptyTrees,
            LossyArg::Nonstandard => LossyClass::Nonstandard,
            LossyArg::All => return None,
        })
    }
}

/// The classes a `git import` line allows, each once, in the order of [`LossyClass::ALL`]: those
/// its `--lossy` names (`all`: every one), but for gitlinks when `--gitlinks` is given, which then
/// decides that class alone.
pub fn lossy_classes(lossy: &[LossyArg], gitlinks: Option<Gitlinks>) -> Vec<LossyClass> {
    let named = |class: LossyClass| {
        (lossy.iter()).any(|&arg| arg == LossyArg::All || arg.class() == Some(class))
    };
    let allowed = |&class: &LossyClass| match gitlinks {
        Some(choice) if class == LossyClass::Gitlinks => choice == Gitlinks::Drop,
        _ => named(class),
    };
    LossyClass::ALL.into_iter().filter(allowed).collect()
}

/// `op`'s operations: the operation log, and undo.
#[derive(Subcommand, Debug, Clone, Copy)]
pub enum OpCommand {
    /// List the store's operations, newest first: each write transaction, its changesets and
    /// its ref moves.
    Log {
        /// List the operations numbered below OP: the last op of a page reads on from there.
        #[arg(long, value_name = "OP")]
        before: Option<u64>,
        /// The most operations to list.
        #[arg(short = 'n', long, value_name = "N", default_value_t = DEFAULT_OP_LOG_LIMIT)]
        limit: usize,
        /// The most changesets to list for each operation.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_MAX_CHANGESETS)]
        max_changesets: usize,
    },
    /// Move every ref operation OP moved back to what it named before, all or none.
    ///
    /// Each ref must still name what the operation set it to (or what it named before, which
    /// is skipped): one that was moved since stops the undo with E_CAS_MISMATCH, and nothing is
    /// written. An undo run again is a replay (already_applied). Changesets are never removed.
    Undo {
        /// The operation, as op log numbers it.
        #[arg(value_name = "OP")]
        op: u64,
    },
}

/// What a command line asks for.
#[derive(Debug)]
pub enum Parsed {
    /// A command to run, and the operation it names.
    Run(Cli, &'static str),
    /// `--help` or `--version`: clap's text to print, which is not an envelope.
    Text(clap::Error),
    /// A usage error, with as much of the line as could be read despite it.
    Usage {
        /// clap's error, for the line with each control character in it escaped.
        error: clap::Error,
        /// The name the line invokes the program by, which clap's usage lines give: the file
        /// name of its first word, `so` or `swarmops`; `swarmops` if that has no UTF-8 file name.
        /// `None` leaves clap's own, `swarmops`.
        bin: Option<String>,
        /// The subcommands named, outermost first, as far as they could be read.
        path: Vec<String>,
        /// The global flags, as far as they could be read.
        global: Global,
    },
}

impl Parsed {
    /// Whether the line runs `swarmops mcp`, as far as it could be read: a command to run, or a
    /// usage error in its arguments. Its stdout belongs to the protocol.
    pub fn runs_mcp(&self) -> bool {
        match self {
            Parsed::Run(_, op) => *op == "mcp",
            Parsed::Usage { path, .. } => path.first().is_some_and(|name| name == "mcp"),
            Parsed::Text(_) => false,
        }
    }
}

/// Reads the command line `args`, the program's name first.
pub fn parse(args: &[OsString]) -> Parsed {
    // Parsing gives `grammar` the name the line invokes the program by, for its usage lines: the
    // file name of the first word, if it is UTF-8. Without one, clap's subcommand usage lines and
    // messages name no program (`Usage: ref set …`), so name it `swarmops` here.
    let mut grammar = command();
    if args
        .first()
        .and_then(|first| Path::new(first).file_name()?.to_str())
        .is_none()
    {
        grammar.set_bin_name(grammar.get_name().to_owned());
    }
    let parsed = grammar
        .try_get_matches_from_mut(args)
        .and_then(|matches| Ok((Cli::from_arg_matches(&matches)?, path(&matches))));
    match parsed {
        Ok((mut cli, path)) => {
            settle_format(&mut cli.global, args);
            // Every leaf names an operation (a unit test checks), so NO_OP is never used here.
            Parsed::Run(cli, op_name(&path).unwrap_or(NO_OP))
        }
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            Parsed::Text(error)
        }
        Err(error) => {
            // Read the line again, ignoring errors, for what it names despite the one found.
            let partial = command().ignore_errors(true).try_get_matches_from(args);
            let (path, mut global) = partial.map_or_else(
                |_| (Vec::new(), Global::default()),
                |matches| {
                    let global = Global::from_arg_matches(innermost(&matches)).unwrap_or_default();
                    (path(&matches), global)
                },
            );
            settle_format(&mut global, args);
            Parsed::Usage {
                error: shown(args, error),
                bin: grammar.get_bin_name().map(str::to_owned),
                path,
                global,
            }
        }
    }
}

/// `error`, which refused the line `args`, as it is to be shown. clap echoes the line's words in
/// its text, and a newline in one would split that text where `commands::usage` reads it by its
/// lines and blank lines. So where a word holds a control character, the error of the line with
/// each such word escaped; `error` itself where none does, or where the escaped line is not
/// refused alike.
fn shown(args: &[OsString], error: clap::Error) -> clap::Error {
    let holds_control = |arg: &OsString| arg.to_string_lossy().chars().any(char::is_control);
    if !args.iter().any(holds_control) {
        return error;
    }
    let escaped = args.iter().map(|arg| {
        if holds_control(arg) {
            OsString::from(printable(&arg.to_string_lossy()))
        } else {
            arg.clone()
        }
    });
    match command().try_get_matches_from(escaped) {
        Err(again) if again.kind() == error.kind() => again,
        _ => error,
    }
}

/// The operation the subcommand path `path` names, as the envelope's `op` gives it: the
/// operations spec's library name (`cat` runs `read_file`), so that a local and a remote
/// store name it alike. `None` for a path that names no operation.
pub fn op_name<S: AsRef<str>>(path: &[S]) -> Option<&'static str> {
    let path: Vec<&str> = path.iter().map(AsRef::as_ref).collect();
    Some(match path[..] {
        ["init"] => "repo_init",
        ["info"] => "repo_info",
        ["whoami"] => "whoami",
        ["schema"] => "schema",
        ["commit"] => "commit",
        ["land"] => "land",
        ["resolve"] => "resolve_rev",
        ["blob", "put"] => "blob_put",
        ["blob", "has"] => "blob_has",
        ["cat"] => "read_file",
        ["ls"] => "read_tree",
        ["show"] => "show",
        ["log"] => "log",
        ["history"] => "path_history",
        ["diff"] => "diff",
        ["ref", "list"] => "ref_list",
        ["ref", "set"] => "ref_set",
        ["ref", "delete"] => "ref_delete",
        ["op", "log"] => "op_log",
        ["op", "undo"] => "undo",
        ["verify"] => "verify",
        ["git", "import"] => "git_import",
        ["git", "export"] => "git_export",
        ["clone"] => "clone_repo",
        ["remote", "add"] => "remote_add",
        ["remote", "remove"] => "remote_remove",
        ["remote", "list"] => "remote_list",
        ["fetch"] => "fetch",
        ["push"] => "push",
        ["sync"] => "sync",
        ["sync", "status"] => "sync_status",
        ["mcp"] => "mcp",
        ["derive"] => "derive",
        _ => return None,
    })
}

/// `cat --lines A-B`: two line numbers, such as 10-20. Whether they make a range `read_file` takes
/// (numbered from 1, ending at or after its start) is the operation's to say, as for every client.
fn line_range(text: &str) -> Result<LineRange, String> {
    let number = |digits: &str| {
        let decimal = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
        decimal.then(|| digits.parse().ok()).flatten()
    };
    let range = text.split_once('-').and_then(|(first, last)| {
        Some(LineRange {
            first: number(first)?,
            last: number(last)?,
        })
    });
    range.ok_or_else(|| "expected two line numbers, A-B, such as 10-20".to_owned())
}

/// The usage line of the subcommand `path` names, or of the deepest part of it that names
/// one, with the program named `bin` as clap's own usage lines name it (see [`Parsed::Usage`]):
/// `Usage: so <path> …`.
pub fn usage(bin: Option<&str>, path: &[String]) -> String {
    let mut at = command();
    if let Some(bin) = bin {
        at.set_bin_name(bin);
    }
    at.build();
    for name in path {
        match at.find_subcommand(name) {
            Some(sub) => at = sub.clone(),
            None => break,
        }
    }
    at.render_usage().to_string()
}

/// The grammar of [`Cli`], with a missing subcommand a usage error rather than a help screen,
/// so that it too gets an envelope.
fn command() -> clap::Command {
    Cli::command()
        .arg_required_else_help(false)
        .mut_subcommand("ref", |cmd| cmd.arg_required_else_help(false))
        .mut_subcommand("blob", |cmd| cmd.arg_required_else_help(false))
        .mut_subcommand("git", |cmd| cmd.arg_required_else_help(false))
        .mut_subcommand("op", |cmd| cmd.arg_required_else_help(false))
        .mut_subcommand("remote", |cmd| cmd.arg_required_else_help(false))
}

/// Of `--json` and `--human`, keeps the one given last: clap's `overrides_with` settles them
/// within one level of the line (before the subcommand, or after it), not across levels.
/// Before a `--`, each such word is the flag itself, since no option takes a value that
/// starts with `--`.
fn settle_format(global: &mut Global, args: &[OsString]) {
    if global.json && global.human {
        let mut human = false;
        for arg in args.iter().skip(1) {
            match arg.to_str() {
                Some("--") => break,
                Some("--human") => human = true,
                Some("--json") => human = false,
                _ => {}
            }
        }
        global.human = human;
        global.json = !human;
    }
}

/// The names of the subcommands `matches` holds, outermost first.
fn path(matches: &ArgMatches) -> Vec<String> {
    let mut path = Vec::new();
    let mut at = matches;
    while let Some((name, sub)) = at.subcommand() {
        path.push(name.to_owned());
        at = sub;
    }
    path
}

/// The matches of the innermost subcommand, which hold the global flags given anywhere.
fn innermost(matches: &ArgMatches) -> &ArgMatches {
    let mut at = matches;
    while let Some((_, sub)) = at.subcommand() {
        at = sub;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &[&str]) -> Vec<OsString> {
        std::iter::once("swarmops")
            .chain(line.iter().copied())
            .map(OsString::from)
            .collect()
    }

    #[test]
    fn the_grammar_is_consistent() {
        command().debug_assert();
    }

    /// Every subcommand without subcommands of its own names an operation, and no two name
    /// the same one.
    #[test]
    fn every_leaf_names_one_operation() {
        fn leaves(cmd: &clap::Command, prefix: &[String], out: &mut Vec<Vec<String>>) {
            for sub in cmd.get_subcommands().filter(|sub| sub.get_name() != "help") {
                let mut path = prefix.to_vec();
                path.push(sub.get_name().to_owned());
                // A group names no operation itself, but `sync` does and has `sync status`.
                if !sub.has_subcommands() || op_name(&path).is_some() {
                    out.push(path.clone());
                }
                if sub.has_subcommands() {
                    leaves(sub, &path, out);
                }
            }
        }
        let mut all = Vec::new();
        leaves(&command(), &[], &mut all);
        let mut ops: Vec<&str> = all
            .iter()
            .map(|path| op_name(path).unwrap_or_else(|| panic!("{path:?} names no op")))
            .collect();
        assert_eq!(ops.len(), 33);
        ops.sort_unstable();
        ops.dedup();
        assert_eq!(ops.len(), 33, "two subcommands name one op");
        assert_eq!(op_name(&["ref"]), None);
        assert_eq!(op_name(&["blob"]), None);
        assert_eq!(op_name(&["git"]), None);
        assert_eq!(op_name(&["op"]), None);
        assert_eq!(op_name(&["remote"]), None);
    }

    #[test]
    fn a_command_line_parses_to_its_operation() {
        let Parsed::Run(cli, op) = parse(&args(&["ref", "set", "main", "abc", "--create"])) else {
            panic!("a valid line");
        };
        assert_eq!(op, "ref_set");
        assert!(matches!(
            cli.command,
            Command::Ref(RefCommand::Set { create: true, .. })
        ));
        // Global flags go before or after the subcommand; --pretty takes its value after `=`.
        let Parsed::Run(cli, _) = parse(&args(&["log", "--repo", "s", "--as", "a/b", "--pretty"]))
        else {
            panic!("a valid line");
        };
        assert_eq!(cli.global.repo.as_deref(), Some("s"));
        assert_eq!(cli.global.agent.as_deref(), Some("a/b"));
        assert_eq!(cli.global.pretty, Some(true));
        // Of --json and --human, the later wins, before the subcommand or after it.
        for (line, human) in [
            (&["--json", "--human", "info"][..], true),
            (&["--human", "--json", "info"], false),
            (&["--json", "--human", "info", "--json"], false),
            (&["--human", "info", "--json"], false),
            (&["--json", "info", "--human"], true),
            (&["info", "--human", "--json"], false),
        ] {
            let Parsed::Run(cli, _) = parse(&args(line)) else {
                panic!("a valid line");
            };
            assert_eq!(
                (cli.global.human, cli.global.json),
                (human, !human),
                "{line:?}"
            );
        }
    }

    #[test]
    fn a_usage_error_keeps_what_the_line_names() {
        let Parsed::Usage { path, global, .. } = parse(&args(&["--human", "ref", "set", "main"]))
        else {
            panic!("a usage error");
        };
        assert_eq!(path, ["ref", "set"]);
        assert!(global.human);
        let Parsed::Usage { path, .. } = parse(&args(&["frobnicate"])) else {
            panic!("a usage error");
        };
        assert_eq!(path, Vec::<String>::new());
        for line in [&[][..], &["ref"], &["blob"], &["git"], &["op"]] {
            let Parsed::Usage { error, .. } = parse(&args(line)) else {
                panic!("{line:?}: a usage error, not a help screen");
            };
            assert_eq!(error.kind(), ErrorKind::MissingSubcommand);
        }
        assert!(matches!(parse(&args(&["--version"])), Parsed::Text(_)));
        assert!(matches!(parse(&args(&["help", "log"])), Parsed::Text(_)));
        // The name the program was invoked by: the file name of the line's first word.
        for (first, name) in [
            ("swarmops", "swarmops"),
            ("so", "so"),
            ("/usr/bin/so", "so"),
        ] {
            let mut line = args(&["log", "-n", "many"]);
            line[0] = first.into();
            let Parsed::Usage { bin, .. } = parse(&line) else {
                panic!("{first}: a usage error");
            };
            assert_eq!(bin.as_deref(), Some(name), "{first}");
        }
        // A first word with no UTF-8 file name (empty, the root, or not UTF-8) leaves the program
        // `swarmops`, in the subcommands' usage lines and messages too.
        let firsts = [
            OsString::new(),
            OsString::from("/"),
            #[cfg(unix)]
            std::os::unix::ffi::OsStringExt::from_vec(vec![0xff]),
        ];
        for first in firsts {
            for (rest, shown) in [
                (&["ref", "set"][..], "Usage: swarmops ref set"),
                (&["ref"], "'swarmops ref' requires a subcommand"),
                (&["info", "--frob"], "Usage: swarmops info"),
            ] {
                let mut line = args(rest);
                line[0] = first.clone();
                let Parsed::Usage { error, bin, .. } = parse(&line) else {
                    panic!("{first:?} {rest:?}: a usage error");
                };
                assert_eq!(bin.as_deref(), Some("swarmops"), "{first:?} {rest:?}");
                assert!(
                    error.render().to_string().contains(shown),
                    "{first:?} {rest:?}"
                );
            }
        }
    }

    /// A `git import` line: its defaults, `--lossy` repeated, a class twice, or `all`, and
    /// `--gitlinks`, which decides the gitlinks class when it is given, whatever `--lossy` says;
    /// a class that is none is a usage error of `git import`.
    #[test]
    fn a_git_import_line_names_its_classes() {
        let parsed = |line: &[&str]| {
            let Parsed::Run(cli, op) = parse(&args(line)) else {
                panic!("{line:?}: a valid line");
            };
            assert_eq!(op, "git_import", "{line:?}");
            let Command::Git(GitCommand::Import {
                path,
                git_ref,
                into,
                lossy,
                gitlinks,
            }) = cli.command
            else {
                panic!("{line:?}: git import");
            };
            (path, git_ref, into, lossy_classes(&lossy, gitlinks))
        };
        let defaults = parsed(&["git", "import", "repo"]);
        assert_eq!(
            defaults,
            ("repo".into(), "HEAD".into(), "main".into(), vec![])
        );
        let named = parsed(&[
            "git",
            "import",
            "repo",
            "--ref",
            "stable",
            "--into",
            "release/x",
            "--lossy",
            "tree_order",
            "--lossy",
            "gitlinks",
            "--lossy",
            "tree_order",
        ]);
        let classes = vec![LossyClass::Gitlinks, LossyClass::TreeOrder];
        assert_eq!(
            named,
            ("repo".into(), "stable".into(), "release/x".into(), classes)
        );
        let classes = |line: &[&str]| parsed(line).3;
        let drop = classes(&["git", "import", "r", "--gitlinks", "drop"]);
        assert_eq!(drop, [LossyClass::Gitlinks]);
        assert_eq!(
            classes(&["git", "import", "r", "--lossy", "all"]),
            LossyClass::ALL
        );
        let all_but = classes(&[
            "git",
            "import",
            "r",
            "--gitlinks",
            "refuse",
            "--lossy",
            "all",
        ]);
        let rest: Vec<LossyClass> = (LossyClass::ALL.into_iter())
            .filter(|&class| class != LossyClass::Gitlinks)
            .collect();
        assert_eq!(all_but, rest);
        let refused = classes(&[
            "git",
            "import",
            "r",
            "--lossy",
            "gitlinks",
            "--gitlinks",
            "refuse",
        ]);
        assert_eq!(refused, Vec::<LossyClass>::new());
        let Parsed::Usage { error, path, .. } =
            parse(&args(&["git", "import", "r", "--lossy", "tree-order"]))
        else {
            panic!("a usage error");
        };
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
        assert_eq!(path, ["git", "import"]);
    }

    /// A `git export` line: its defaults (`main` into `main`, not forced), and `--ref`, `--branch`
    /// and `--force` given.
    #[test]
    fn a_git_export_line_takes_its_defaults() {
        let parsed = |line: &[&str]| {
            let Parsed::Run(cli, op) = parse(&args(line)) else {
                panic!("{line:?}: a valid line");
            };
            assert_eq!(op, "git_export", "{line:?}");
            let Command::Git(GitCommand::Export {
                path,
                source_ref,
                branch,
                force,
            }) = cli.command
            else {
                panic!("{line:?}: git export");
            };
            (path, source_ref, branch, force)
        };
        let defaults = parsed(&["git", "export", "repo.git"]);
        let expected = ("repo.git".into(), "main".into(), "main".into(), false);
        assert_eq!(defaults, expected);
        let line = [
            "git",
            "export",
            "repo.git",
            "--ref",
            "release/x",
            "--branch",
            "stable",
            "--force",
        ];
        let expected = ("repo.git".into(), "release/x".into(), "stable".into(), true);
        assert_eq!(parsed(&line), expected);
    }

    #[test]
    fn a_line_that_runs_mcp_is_known_as_one() {
        for line in [
            &["mcp"][..],
            &["--repo", "s", "mcp"],
            &["mcp", "--frob"],
            &["mcp", "x"],
        ] {
            assert!(parse(&args(line)).runs_mcp(), "{line:?}");
        }
        for line in [&["info"][..], &["mcp", "--help"], &["frobnicate"], &[]] {
            assert!(!parse(&args(line)).runs_mcp(), "{line:?}");
        }
    }

    #[test]
    fn usage_lines_name_the_subcommand() {
        assert_eq!(
            usage(None, &["log".to_owned()]),
            "Usage: swarmops log [OPTIONS] [REV]"
        );
        assert!(usage(None, &[]).starts_with("Usage: swarmops [OPTIONS] <COMMAND>"));
        let set = usage(None, &["ref".to_owned(), "set".to_owned(), "x".to_owned()]);
        assert!(set.starts_with("Usage: swarmops ref set "), "{set}");
        // The program by the name the line invoked it by.
        assert_eq!(
            usage(Some("so"), &["log".to_owned()]),
            "Usage: so log [OPTIONS] [REV]"
        );
        let set = usage(Some("so"), &["ref".to_owned(), "set".to_owned()]);
        assert!(set.starts_with("Usage: so ref set "), "{set}");
    }
}
