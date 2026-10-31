//! [`LocalOps`]: every [`StoreOps`] operation, run in this process against a store on the
//! local filesystem through `swarmops-repo` (operations spec §1, §7). Its documentation says
//! what each operation does and how failures map to error codes.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Map, Value};
use swarmops_api::VERIFY_MAX_FINDINGS;
use swarmops_api::{
    Attribution, ChangeOp, ChangeOut, ChangesetDetail, ChangesetSummary, CommitReq, CommitResp,
    DeriveReq, DeriveResp, Diff, DiffFormat, DiffHunk, DiffLineOut, DiffReq, DiffStatus,
    EntryKindOut, ErrorCode, FileContent, FileDiff, FileKind, Identity, InfoReq, LineOp, LineRange,
    LogCutOut, LogPage, LogReq, MAX_TREE_DEPTH, OpChangesetOut, OpError, OpLogPage, OpLogReq,
    OperationKind, OperationOut, PathHistoryEntry, PathHistoryPage, PathHistoryReq, ReadFileReq,
    ReadTreeReq, RecoveredOut, RefDeleteReq, RefListReq, RefListResp, RefMoveOut, RefOut,
    RefSetReq, RefSetResp, RepoInfo, Rev, ShowReq, SignatureOut, Signatures, StoreOps,
    TreeEntryOut, TreeListing, UndoReq, UndoResp, VerifyFinding, VerifyReport, VerifyReq,
};
use swarmops_api::{BlobGetReq, BlobGetResp, RebaseReq, RebaseResp};
use swarmops_api::{BlobHasReq, BlobHasResp, BlobPutReq, BlobPutResp};
use swarmops_api::{ContentInput, ModeInput, ResolutionInput, ResolutionsInput};
use swarmops_api::{GcReq, GcResp, RepairReq, RepairResp, UpgradeReq, UpgradeResp};
use swarmops_api::{LandReq, LandResp, LandStatus, MAX_LAND_ATTEMPTS};
use swarmops_api::{ResolveRevReq, ResolvedRev, RevKind};
use swarmops_core::bytes;
use swarmops_core::{
    ChangesetId, DeltaEntry, DeltaOp, EntryKind, EntryTarget, LeafKind, ObjectId, RepoPath,
    Signature,
};
use swarmops_git::MemoKey;
use swarmops_repo::LandRequest;
use swarmops_repo::land::Gated;
use swarmops_repo::merge::line_diff::{self, DiffLine, UnifiedHunk};
use swarmops_repo::merge::tree_diff::{ChangeKind, Leaf, PathFilter, TreeChange};
use swarmops_repo::{
    CasResult, CommitRequest, Depth, FileSpan, ListedEntry, LocalRepo, MismatchCause, OpId,
    OpIdError, OpRecord, Operation, PathEvent, RepoError, RepoRead, StoredChangeset, VerifyProblem,
};
use swarmops_repo::{Resolution, Resolutions};
use swarmops_store::external_ids;
use swarmops_store::{FsObjectStore, LogCut, Recovered, RefKind, StoreError, StoreFormat};

use crate::authors;
use crate::content;
use crate::error::{self, During};

/// The agent a changeset is attributed to when its metadata names none.
pub(crate) const UNKNOWN_AGENT: &str = "unknown/unknown";

/// A snapshot of the store's metadata, in which a request makes all its reads.
type Snapshot<'a> = RepoRead<'a, FsObjectStore>;

/// The metadata key that names who landed a changeset: every changeset a land makes carries it
/// (M4's `land`), and the operation log tells a land from a publish by it.
const LANDER_KEY: &str = "lander";

/// The metadata key of a land's idempotency key, when it had one.
const LAND_KEY_KEY: &str = "land_key";

/// The metadata key of the run of who landed a changeset, when the lander has a run id.
const LANDER_RUN_ID_KEY: &str = "lander_run_id";

/// The metadata key of the chain a land squashed: the changesets' ids, oldest first, in hex, a
/// space between each two.
const SQUASHED_FROM_KEY: &str = "squashed_from";

/// The longest land key: 64 characters, a changeset id's hex digits (M11's form).
const MAX_LAND_KEY: usize = 64;

/// The operations over a store on the local filesystem: [`StoreOps`], run in this process with
/// the repository library.
///
/// # Revisions
///
/// Each revision a request names is resolved once, by the repository's grammar (`main`,
/// `release/<name>`, a full changeset id or a unique prefix of 12 or more of its hex digits,
/// then any `~N` suffixes), and the operation works on the changeset it named. A request made
/// through [`LocalOps::recording`] records what each revision named, and `main` when an
/// operation used it by default, in the [`Resolved`] of that request alone, which the CLI and
/// the MCP server report as `meta.resolved`: requests that share a `LocalOps`, from one thread
/// or from several, never see each other's. An operation called on `LocalOps` itself records
/// nothing.
///
/// A request makes all its reads in one snapshot of the store's metadata, its own, taken when the
/// operation starts (storage spec §8): the revisions and refs it names resolve in the same view
/// (operations spec §4), and a `commit` checks its parent in the view it resolved it in. No
/// operation reads in its snapshot after its own write.
///
/// # Recovery
///
/// A write first repairs what earlier writers left when they crashed or failed after their
/// commit point (storage spec §3). `commit`, `ref_set`, `ref_delete` and `undo` made through
/// [`LocalOps::recording`] record what their write repaired, when it repaired anything, in the
/// request's [`Resolved`], which the CLI and the MCP server report as `meta.recovered`: on
/// success and on `E_CAS_MISMATCH` alike, never as a warning, and not when the operation then
/// fails otherwise. `git_import` records what its writes repaired, added up, on success only;
/// `git_export`, what the tail check of its committed snapshot repaired (frames listed or written
/// afresh, a torn tail cut), on success only.
///
/// # Operations
///
/// - `commit` builds on `parent`, or, when it is omitted, on `main`'s head if `main` exists and
///   on nothing otherwise (a root changeset). The changeset is the caller's: author and
///   committer are both the agent name with the email `<instance>@<family>.swarmops.local` at
///   the current time, UTC (or at [`NOW_ENV`]'s, a test hook of builds with the `test-hooks`
///   feature); its metadata holds `agent` and, when the caller has one, `run_id`.
///   `files_changed` counts the changeset's delta, which leaves out puts of what a path holds
///   already. The change list follows [`crate::content`]'s rules. `file` content is read here,
///   in the caller's process: `LocalOps` runs on the client. A server that exposes
///   [`StoreOps`] over a wire must refuse [`ContentInput::File`](swarmops_api::ContentInput)
///   instead of reading a path its caller names (the first such server, M11's `serve`, comes
///   with the client-side pass that turns such content into bytes before a request leaves the
///   client); `swarmops mcp` is no such server,
///   since its caller starts it, as its own user, to serve that caller alone over stdio, and it
///   reads such content as the CLI does. A metadata key may not end in
///   `:b64`, which `show` reserves (`E_INVALID_ARG`).
///   With `advance`, the commit also moves that ref to its changeset, in the same transaction,
///   if the ref names the commit's parent (for a root commit, if the ref does not exist):
///   `advanced` reports the move, `already_applied` where an identical request made it before.
///   A ref that names anything else fails the commit with `E_CAS_MISMATCH`, and nothing is
///   recorded. Without `parent`, such a commit builds on that ref's head, recorded in the
///   request's [`Resolved`] under the ref's name, and on nothing if the ref does not exist (a
///   root changeset that creates it): the move then only detects a race from that moment on.
///   A change's `blob` content names a blob the store holds by its id: the commit neither reads
///   nor writes it again, and an id the store holds no blob under is `E_INVALID_ARG`.
///   With `idempotency_key`, a uuid v7 checked before anything is resolved (anything else is
///   `E_INVALID_ARG`, naming it in `details.idempotency_key`), the commit signs at the key's
///   time, to the second, whatever the clock and [`NOW_ENV`] say, and its metadata holds the
///   key, in lowercase, as `commit_key`: the same request on the same parent is then the same
///   changeset. A commit whose changeset the store held already, and which moved no ref, wrote
///   nothing, and says `replayed`. A keyed commit whose parent (given, or the head it builds on
///   by default, which another request may have moved there) is the changeset its key made
///   replays that changeset rather than build on it, once a write transaction has confirmed
///   under the store's lock that the changeset is there (with `advance`, that the ref names it;
///   a ref that names another is `E_CAS_MISMATCH`). A keyed commit without `parent` builds on
///   the head as it is when it runs, so a retry after that head moved elsewhere commits again.
/// - `land` merges `head`'s change, its first-parent chain after `base` (when omitted, the merge
///   base of `head` and the target's head, along first parents), three-way onto `target` (`main` or
///   `release/<name>`, which must exist), and publishes the result there as one new changeset,
///   whose parent is the target's head, with the target's move to it, in one transaction
///   (operations spec §3, the store-only block). The changeset's author is `head`'s and its
///   committer the caller, signed as a commit is; its metadata holds `head`'s `agent` and `run_id`,
///   the caller's agent as `lander` and run id as `lander_run_id`, the key as `land_key` and the
///   chain as `squashed_from`. A target that moved since the land read it makes it merge again onto
///   the new head and retry, `max_attempts` times at most. A keyed land run again finds the
///   changeset it landed on the target and replays it (`replayed`), decided under the store's lock,
///   as a keyed commit's replay is. A key that is not 1 to 64 letters, digits and `-` is
///   `E_INVALID_ARG`, naming it in `details.idempotency_key`, and so is a `max_attempts` outside 1
///   to 32 (`details.max_attempts`), both checked before anything is resolved. What `head`, `base`
///   and the target name in the request's snapshot is recorded in its [`Resolved`]; each attempt
///   reads the target again, and `rebased_onto` says which head the change landed on. With
///   `resolutions` (operations spec §3, resolution path (b)), each path is a repository path and
///   each resolution exactly one of `take`, `content` (a `mode` only beside it) and `delete`, also
///   checked before anything is resolved (`E_INVALID_ARG`, naming the path in `details.path`);
///   `against` resolves in the request's snapshot, recorded with the rest, and the contents follow
///   the change list's rules, each error said of its resolution's path. Each attempt applies a
///   resolution while the target holds its path as `against` does; a path it holds otherwise comes
///   back in the `E_CONFLICT` report's `stale`.
/// - `blob_put` stores its content as a blob, by the change list's rules for content (`blob`
///   content aside, which names a blob already stored), durable when it returns, and says
///   whether it placed it (`written`). It needs an identity, which it records nowhere.
/// - `blob_has` gives the ids, of those asked for, that the store holds a blob under, in
///   lowercase, in the order first given and each once; a tree's or a changeset's id is not
///   among them.
/// - `resolve_rev` names the changeset a revision names, resolved as every operation resolves
///   one, with its kind: `ref` when the revision is exactly the name of a ref (whose target the
///   id is now), `changeset` for any other revision (an id, a prefix, a `~N` walk).
/// - `read_file` returns at most `max_bytes` bytes of the file: from `offset` (0, its start, by
///   default), or of the lines `lines` names (numbered from 1, both ends included: those that
///   exist). They are UTF-8 text when they are text, a cut inside a character moved back to the
///   character's start (so up to 3 bytes fewer); otherwise exactly `max_bytes` bytes, in base64,
///   as are bytes from an offset inside a character. A read cut short gives `next_offset`, where
///   the next page starts; an offset past the end gives empty content, not cut short. The blob's
///   object file is read whole and verified whole, but only the bytes returned are kept. Both
///   `offset` and `lines`, and lines from 0 or a range that ends before it starts, are
///   `E_INVALID_ARG`. A directory there is `E_IS_DIR`, nothing there `E_PATH_NOT_FOUND`.
/// - `read_tree` lists a directory (the root for an omitted or empty path; one trailing `/` is
///   taken) and, to `depth` levels (1 to 64, else `E_INVALID_ARG`), the directories below it, in
///   pre-order (a directory, then the entries below it; each directory's entries in name order),
///   each leaf with its size from its object's header: a page of at most `limit` entries after
///   the `cursor` path, those past `depth` 1 with their `path`. A cursor that is no repository
///   path is `E_INVALID_ARG`; a file or nothing at the path, `E_PATH_NOT_FOUND`.
/// - `show` attributes the changeset by its `agent` and `run_id` metadata (`unknown/unknown`
///   when it records no agent); `created_at` is the committer's time in milliseconds (0 before
///   1970); a metadata value that is not UTF-8 is given in base64 under its key with `:b64`
///   appended, a suffix commits refuse in a key, so `k:b64` is always the base64 of `k`'s
///   value; `changes` holds at most `max_changes` entries. A changeset a land made also names
///   its lander (its `lander` and `lander_run_id` metadata), its `land_key` and the chain it
///   squashed (`squashed_from`, oldest first).
/// - `log` follows first parents from `rev` (`main` when omitted) or, given a `cursor`, from the
///   first parent of the changeset it names, which must be a full id the store holds; a page
///   is `truncated` when older history follows its last entry. With `paths` (each a repository
///   path, one trailing `/` taken), it lists only the changesets whose delta has an entry at one
///   of them or below it, by the chase of each path's last modifier (the derived value
///   `LastModified`), and a page is `truncated` exactly when another such changeset follows.
/// - `path_history` lists the changesets that changed what `path` holds (a file's content or
///   kind, or anything below a directory; one trailing `/` taken), newest first, from `rev`
///   (`main` when omitted) or after a `cursor` as `log` does, each with what the path holds after
///   it (`put` with its kind and id, `delete` with neither) and the changeset's agent, time and
///   title. A path that no changeset on the first-parent chain touched is `E_PATH_NOT_FOUND` on
///   a first page; a page after a cursor may be empty.
/// - `diff` lists the files that differ between the trees of the changesets `from` and `to`
///   name, in path order (by bytes): each file, executable or symlink that is on one side only
///   (`added`, `deleted`), whose content or exec bit changed (`modified`, a symlink's target
///   too), or that turned from a symlink into a file or back (`type_changed`); a directory as
///   its files, and no renames. `paths` keeps the files at or below one of them, each a
///   repository path with one trailing `/` taken. A page holds at most `limit` files, after
///   `cursor`, the last path of the page before. `names` gives each file's path and status;
///   `stat` reads its two blobs and adds whether it is binary (a side that is not UTF-8, or
///   holds a NUL in its first 8,000 bytes) and, for text, the lines it inserts and deletes;
///   `unified` adds its kinds and blob ids and, for text, its hunks, with `context` unchanged
///   lines around each change. A `unified` page stops before the first file that would take
///   its files, as the JSON array they print as, past `max_bytes`; its first file comes all
///   the same, its hunks cut to the first ones that fit (`hunks_truncated`) when they alone
///   would pass it. Text with a side past the line diff's bound (more than 49,152 lines or
///   8 MiB) gets no line diff: no line counts, and `hunks_truncated`, `unified` with no hunk.
/// - `ref_set` and `ref_delete` move a ref by compare-and-swap, recorded for the caller's agent,
///   and decide every move under the store's lock, after the write's tail check, never from a
///   reader's snapshot. A move that finds the ref as it asks already (at `new` for `ref_set`,
///   whatever it expected; gone, or never there, for `ref_delete`) is a replay: `already_applied`,
///   with the response the move gave, and nothing is written, no journal frame and no new version
///   of the ref. So a `ref_set` whose `new` is the target it expects never moves the ref: it is a
///   replay where the ref holds that target, and a mismatch where it holds another.
/// - `verify` checks the store and lists each problem as one line.
/// - `derive` derives ahead of any reader the eager derived values (storage spec §5) of the
///   changesets recorded in the journal's frames after frame `after` (from the first frame
///   without it), reading at most `limit` frames, and gives the last frame it read as `next`,
///   omitted when it read none; with a `limit` of 0 it reads no frame, derives nothing and gives
///   the journal's last frame. It writes only the store's cache of derived values, so it needs
///   no identity.
/// - `git_import` is client-side, as `init` is, and so no [`StoreOps`] method
///   ([`LocalOps::git_import`], [`Recording::git_import`]): it reads the git repository at
///   `git_path` on this host (a relative path from the working directory) and brings every
///   commit `git_ref` reaches that the store does not hold into it, parents first, then moves
///   `target_ref` to the changeset of `git_ref`'s commit by compare-and-swap, recorded for the
///   caller's agent (storage spec §7). The target is checked as a ref name before the
///   repository is opened. An imported changeset has no `agent` metadata: its author is its git
///   author, in `signatures`, and `show` attributes it to `unknown/unknown` (the M3 plan's
///   decision 15).
/// - `git_export` is client-side too ([`LocalOps::git_export`], [`Recording::git_export`]): it
///   writes into the git repository at `git_path` on this host (a relative path from the working
///   directory; one that exists, which `git init --bare` makes) every commit of `source_ref`'s
///   history that the branch `branch`'s last export does not reach, parents first, makes them
///   durable, records the oids of the commits made in the store in the external id index, then
///   moves the branch to the export of the ref's head by compare-and-swap (storage spec §7). It
///   reads the head in a committed snapshot, taken under the store's lock after the journal's tail
///   check, and records it under the ref's name as a revision it resolved. A changeset imported
///   from git exports to the commit it came from, one made in the store to a commit with its own
///   signatures and `SwarmOps-*` trailers. It needs no identity: it writes no journal frame, only
///   the git repository and the store's caches (derived values, external id entries). The
///   export oids it compares the branch with, names as parents and moves the branch to are
///   derived from the store's own objects, not read from the derivation of export oids that
///   every `LocalOps` has registered, and each memo it meets is checked against them; given
///   the store's memo key ([`LocalOps::with_memo_key`]), a memo whose MAC under it checks is
///   taken instead, and the history below it is not read. A memo that disagrees, or whose MAC
///   does not check, is `E_STORE_CORRUPT`.
/// - `op_log` lists the store's operations, newest first, below `before`: each one write
///   transaction, a journal frame numbered from 0 in commit order, with its kind (read from its
///   records: one changeset and a move of a ref to it is a `land` when the changeset's metadata
///   names a lander, else a `publish`; changesets alone a `commit`; ref moves alone a
///   `ref_move`; a compaction marker alone a `compaction`; anything else `mixed`), at most
///   `max_changesets` of its changesets (each read from its object for its agent, time, title
///   and land key; `changesets_total` counts them all) and its ref moves. A page is `truncated`
///   when older operations follow its last.
/// - `undo` moves every ref an operation moved back to what it named before the operation, for
///   the caller's agent, by compare-and-swap in one transaction of its own, all of them or none:
///   a ref that names that already is skipped, and one that names neither that nor what the
///   operation set it to is `E_CAS_MISMATCH`, with nothing written. With every ref skipped,
///   nothing is written: `already_applied`. Its `op` is the operation it recorded, which
///   another undo can take back.
///
/// Limits of 0 (`limit`, `max_changes`, and the `max_bytes` of `read_file` and `diff`) are
/// `E_INVALID_ARG`, but for `derive`'s `limit`.
///
/// # Errors
///
/// Every error's message and hint is one line (control characters in it are escaped), and the
/// hint, where there is an obvious next step, is a command, each word a shell would split or
/// expand in single quotes.
///
/// The error enums are `#[non_exhaustive]`, so the compiler no longer checks that every variant
/// has a row: a variant this build does not know is `E_INTERNAL`, its hint says to report it.
/// Repository failures map to codes as follows, `details` keys in braces:
///
/// - `E_REV_NOT_FOUND` `{rev}`: `NotFound`, and the store's `UnknownRefTarget`. The hint is
///   `swarmops ref list` for a ref name, `swarmops log` for an id or a `~N` walk.
/// - `E_AMBIGUOUS_ID` `{rev}`: `Ambiguous`; the hint says to give more digits.
/// - `E_INVALID_ARG`: `BadRev` `{rev}` (the hint gives the revision grammar),
///   `DuplicatePath` `{path}`, `ReservedMetadataKey` `{key}` (a metadata key that ends in
///   `:b64`), and the store's `DuplicateParent` `{parent}`, `Limit`
///   `{store_limit, max, got}` (a request past a limit of the store's layout: `store_limit` is
///   `parents`, `actor_bytes`, `ref_name_bytes`, `records` or `repo_name_bytes`; the object limit
///   is `E_TOO_LARGE`, below) and `BadPrefix`
///   `{prefix}` (an id prefix that is not 1 to 64 hex digits). Of the requests of these
///   operations only `init`'s reaches either, with a name of more than 255 bytes
///   (`repo_name_bytes`): a commit has one parent, the identity bounds the actor, and revision
///   resolution takes 12 to 63 hex digits. And `MissingBlob` `{index, path, blob}`: a change
///   that names by id a blob the store does not hold (no object, or a tree or a changeset), a
///   mistake in the request rather than damage; its hint is `swarmops blob put <file>`. A land's
///   resolution that names such a blob is `MissingBlob` `{path, blob}`. Its `blob` is the id in
///   lowercase, however the request spelled it; an id that is not 64 hex digits is
///   `E_INVALID_ARG` `{blob}` with the id as the request sent it.
/// - `E_TOO_LARGE` `{store_limit, max, got}`: the store's `Limit` of an object longer than an
///   object holds (`store_limit` `object_bytes`, `max` 512 MiB): a commit's tree or changeset
///   object past that size, since a put's content is refused at it first (`{index, path, size,
///   limit, scope}`, below).
/// - `E_EXISTS`: `Exists`; the hint is `swarmops --repo <path> info`.
/// - `E_UNKNOWN_PARENT` `{parent}`: `UnknownParent`; the hint is `swarmops log`.
/// - `E_EMPTY_COMMIT`: `EmptyChanges`; the hint names `--allow-empty` and its MCP argument,
///   `allow_empty`.
/// - `E_TYPE_CONFLICT` `{path}`: `TypeConflict`.
/// - `E_PATH_NOT_FOUND` `{path}`: `PathNotFound`.
/// - `E_REF_NAME_INVALID` `{name}`: `RefName`, the repository's or the store's; the hint gives
///   the ref grammar.
/// - `E_CAS_MISMATCH` (retryable) `{name, actual, committed}`, `committed` always `false`:
///   `RefMoved`, a `commit` with `advance` whose ref named something else than the commit's
///   parent (`actual` null when the ref does not exist), so that the commit recorded nothing.
///   The message says to commit the change again on top of `actual` (`--parent <actual>
///   --advance <name>`), and the hint only reads, as for a ref move's below.
/// - `E_CAS_MISMATCH` (retryable) `{name, actual, expected, committed}`, `committed` always
///   `false`: `CasMismatch`, a land with `expect_target` whose target named `actual`, not
///   `expected`, before it merged or when its one publish lost the race, so that it landed
///   nothing. The message says to rebase the change onto `actual`, check it and land it again;
///   the hint only reads, as for `RefMoved`.
/// - `E_CAS_MISMATCH` (retryable) `{name, actual, op}`: `UndoBlocked`, an `undo` of operation
///   `op` that found ref `name` at `actual` (null when it does not exist), moved since the
///   operation, so that the undo wrote nothing. The message says to undo the operations that
///   moved it since, newest first, or to land a change that reverses the operation; the hint is
///   `swarmops op log`, which only reads.
/// - `E_OP_NOT_FOUND` `{op, id, last, reason}`: `OpNotFound`, an `undo` of id `id` (as the
///   request gave it), numbered `op`, past the store's last (`reason` `past_end`, `last` null
///   when the store has none) or whose crc is not the frame's at `op` (`checksum`: the operation
///   named was taken back and its number went to another, or the id is mistyped); the frame's id
///   is never given. The hint is `swarmops op log`.
/// - `E_NOTHING_TO_UNDO` `{op}`: `NothingToUndo`, an `undo` of an operation that left every ref
///   where it found it; the hint is `swarmops op log`.
/// - `E_CONFLICT` `{report}` (exit 3): `Conflict`, a land whose change conflicts with the target's
///   head, which recorded nothing. `report` is the conflict report (operations spec §3): each
///   conflicting file with its kind, its blobs, its hunks (each side's lines numbered from 1, both
///   ends included, and their texts while they fit within the request's `max_conflict_bytes`, else
///   `truncated`) and the target's changeset and agent that last changed it, and the paths whose
///   resolutions did not apply (`stale`). The hint reads the target's side of the first
///   conflicting path, then names the land again with its resolutions.
/// - `E_NOTHING_TO_LAND` `{reason}`: `NothingToLand`, a land whose head is on the target
///   (`on_target`) or whose change the target holds already (`no_changes`).
/// - `E_CAS_RETRY_EXHAUSTED` (retryable) `{attempts, actual}`: `RetryExhausted`, a land whose
///   target moved before each of its attempts could publish, which recorded nothing; the hint says
///   to run it again.
/// - `E_INVALID_ARG`, of a land: `KeyReused` `{key, landed, target}` (a key that landed another
///   change, or landed on another ref, `target`; the hint is `swarmops show <landed>`),
///   `MergeChangeset` `{changeset}`, `BaseNotAncestor` `{base, head}` and `NoMergeBase`
///   `{head, theirs}` (the hint is `swarmops log <head>`).
/// - `E_BUSY` (retryable): the store's `Busy` (a writer's lock wait ran out) or
///   `SnapshotContended` (a read's snapshot rounds ran out); either way nothing was committed (a
///   commit's flushed objects may remain, for GC). The hint says which, and to retry.
/// - `E_NOT_A_REPO`, while opening: the store's `Format`, and an `Io` error of kind
///   `NotADirectory`. The hint is `swarmops init <path>` when the path holds no `FORMAT` file;
///   with one, the store is damaged or of a layout this build does not read.
/// - `E_STORE_CORRUPT`: `MissingObject`, and the store's `Corrupt` and `BadHeader`
///   `{object}`; `Codec`, and the store's `Codec`, `JournalCorrupt`, `IndexCorrupt`,
///   `IndexMismatch` and `MissingParent`; an `Io` error of kind `InvalidData` (a malformed
///   metadata file) `{fs_op, store_file}`; and the store's `Format` other than while opening.
///   Once the store is open, the hint is `swarmops verify` (with `--deep` for an object whose
///   bytes fail their id).
/// - `E_UNSUPPORTED_FS` `{path}`: the store's `UnsupportedFs`, a path under `/mnt/`, on `init`
///   and on open alike; the hint points at a native filesystem.
/// - `E_IO` `{fs_op, store_file}`: any other `Io` error, whatever its kind (an OS `EINVAL` among
///   them). `fs_op` names the file operation that failed; `store_file` the file of the store it
///   was on, relative to the store's root (`.` for the root itself), and is absent for a path
///   outside it; no such error carries a host path but the two operations spec §0 names: a
///   symlink loop met while working out where the store's path leads (`fs_op` `resolve`, no
///   `store_file`), whose message names that path, and tempfile's giving up after 65,536 taken
///   temp-file names. Also the store's `NotCommitted` (below) whose
///   append the filesystem refused for want of permission or as a read-only filesystem
///   (`PermissionDenied`, `ReadOnlyFilesystem`), `{committed, fs_op, store_file}`: running the
///   same command again meets the same refusal until its cause is fixed, which the hint names.
/// - `E_IO_TRANSIENT` (retryable) `{committed, fs_op, store_file}`, `committed` always `false`:
///   the store's `NotCommitted` other than the two refusals `E_IO` takes (above): a journal
///   append that failed and that the store cut back off the journal, so the write committed
///   nothing; `fs_op` and `store_file` name the append. The hint says to run the same command
///   again.
/// - `E_INTERNAL`: the store's `PostCommit`, which the repository turns into a warning (a
///   failure after the commit point, a failed listing in `journal.off` among them), so that it
///   never reaches here; the store's `BadCall`, a call its API does not take, which only a bug
///   makes; and `Derived`, a derived value that cannot be made (a cycle among the values it
///   needs, a key or a value that does not encode or decode, a derivation not registered),
///   which only a bug of the derivation makes, as its inputs are immutable. And `Internal` and the
///   store's `Internal`, a variant of another crate's enum this build does not know, a bug.
///
/// The operations raise their own errors besides:
///
/// - `E_IS_DIR` and `E_PATH_NOT_FOUND` `{rev, path}` for reads;
/// - `E_CAS_MISMATCH` `{name, actual}` (`actual` null when the ref does not exist), whose hint
///   only reads, `swarmops show <actual>` or `swarmops ref list`, so that an agent that runs it
///   cannot undo the move that won, and whose message says what to do next:
///   - to a `ref_set` that finds the ref moved, or created by another: commit its change again
///     on top of `actual` (`--parent <actual>`), re-applying its edits to the content there
///     rather than re-sending its old files, then retry with `--expect <actual>`;
///   - to a `ref_delete`: re-check before deleting;
///   - to a move of a ref that is gone: that it is gone;
/// - `E_TOO_LARGE` `{index, path, size, limit, scope}` (and the client `file` for file content) for
///   a put over 512 MiB (`scope` `put`, measured as each content is converted, in list order) or a
///   commit's contents over 1 GiB together (`commit`, whose `size` is theirs up to and with the
///   change's), the second measured by the sizes the contents declare, before any is read;
/// - `E_INVALID_ARG` for limits, cursors and requests that break their own rules.
///
/// `git_import`'s failures map as follows, the source repository named by the path the request
/// gave and its objects by their git ids:
///
/// - `E_GIT_NOT_A_REPO` `{git_path}`: no git repository gix can open at that path.
/// - `E_GIT_REF_NOT_FOUND` `{git_ref, kind?}`: `git_ref` names nothing in it, or (`kind`) an
///   object that is no commit; the hint lists its refs (`git -C <path> for-each-ref`).
/// - `E_GIT_UNSUPPORTED` `{reason, …}`: a repository the import does not read as it is, refused
///   before anything past its configuration is read: `reason` `object_format`
///   `{object_format}`, `ref_storage` `{ref_storage}`, or `shallow`, whose hint deepens it
///   (`git -C <path> fetch --unshallow`); `special_file` `{file}`, a FIFO, a socket or a device
///   (or a symlink to one) where it reads a file of the git directory, named by its path there
///   and refused before any file is read; `file_too_long` `{file, size, max}`, a file it reads
///   whole there (a ref, `config`, `config.worktree`, `objects/info/alternates`) longer than
///   16 MiB, refused unread; or a commit no lossy class covers: `parents`
///   `{commit, parents, max}`, more than 255 of them, or `mode` `{commit, path, mode}`.
/// - `E_GIT_READ` `{reason, object?}`: an object the repository lacks (`missing`, as a partial
///   clone lacks blobs), whose bytes are not its id's (`corrupt`) or do not parse (`malformed`),
///   each with the hint `git -C <path> fsck`; or a read that failed (`io`).
/// - `E_GIT_IMPORT_LOSSY` `{commit, class, path, path_base64?}`: content of a lossy class the
///   request does not allow; `path` is empty for `nonstandard`, and a path that is not UTF-8 is
///   given in base64 too. The hint names `--lossy <class>`.
/// - `E_TOO_LARGE` `{commit, path, size, max}`: a blob longer than an object holds, refused by
///   its header before it is read; `{object, kind, size, max}`: a commit, a tree or an annotated
///   tag (`kind`) longer than that, refused by its header the same way, before a byte of it is
///   read whole to be parsed.
/// - `E_GIT_IMPORT_DIVERGED` (exit 3) `{target_ref, target_head, git_head, base?, external_id?}`:
///   the git commit the target's head stands for (`base`: the one it holds, or for a native head
///   the one it exports to) is not one that `git_ref`'s commit reaches; nothing was written. The
///   hint is the target's log, or a deep verify, which reports it, where the import passed over
///   an external id entry whose changeset stands for another commit (`external_id`
///   `{commit, changeset}`).
/// - `E_CAS_MISMATCH` (retryable) `{name, actual}`: the target moved during the import, with
///   every commit recorded; the same import run again moves it if what it names leads to the
///   tip. The hint only reads, as for a ref move.
/// - `E_REF_NAME_INVALID` `{name}`: a target that is no ref name.
/// - A store failure is the code its repository error has above (`E_BUSY` after three retries,
///   `E_IO_TRANSIENT`, `E_IO`, `E_STORE_CORRUPT`, …), and a frame that committed but failed
///   after its commit point is `E_IO` `{committed: true, warnings}`; either way the import
///   stopped there, and `{imported, last_commit}` say how many commits the frames that
///   committed recorded and the last of them (null for none). The hint is to run the same import
///   again, which continues after them, unless the store is damaged (`swarmops verify`), and it
///   names first what to fix where the filesystem refused a journal append for good (`E_IO`
///   `{committed: false}`, as above).
/// - `E_INTERNAL` `{commit}`: a commit whose conversion disagrees with its own check, a bug.
///
/// `git_export`'s failures map as follows, the repository named by the path the request gave,
/// its files relative to its git directory and its objects by their git ids:
///
/// - `E_REF_NAME_INVALID` `{name}`: a source ref that is no ref name, before the repository is
///   opened; or a branch name git does not take, whose hint is `git check-ref-format --branch`.
/// - `E_GIT_NOT_A_REPO` `{git_path}`: no git repository gix can open at that path; the hint makes
///   one (`git init --bare <path>`).
/// - `E_GIT_UNSUPPORTED` `{reason, …}`: a repository the export does not write as it is, as the
///   import refuses a source: `object_format`, `ref_storage`, `shallow`, `special_file` or
///   `file_too_long`.
/// - `E_INVALID_ARG` `{reason: "checked_out", branch}`: the branch `HEAD` names in a repository
///   with a work tree, refused with or without `force`, before the store is read; the hint
///   detaches `HEAD` or names another branch.
/// - `E_REV_NOT_FOUND` `{rev}`: a source ref the store does not have; the hint lists the refs.
/// - `E_GIT_EXPORT_DIVERGED` (exit 3) `{reason, branch, git_head, head_changeset,
///   exported_changeset?}`: the branch names no export of the source's head or of an ancestor of
///   it, and nothing was written; `reason` says why: `branch_ahead` (commits made in git on the
///   head's export: the hint imports them), `both_moved` (on an older export, while the source
///   moved too), `branch_moved` (a commit of no export of the history), `source_moved_back` (the
///   export of a changeset that is no ancestor of the head: the hint is `swarmops op log`),
///   `lossy_history` (the commits a lossy import was made of) or `moved_during_export` (another
///   writer moved the branch while the export wrote, so what it wrote stays; `git_head` null when
///   it deleted it). `exported_changeset` names the changeset the branch holds the export of,
///   and the hints but the last name `--force`, which moves the branch all the same, by
///   compare-and-swap against the commit the export read.
/// - `E_GIT_WRITE` `{reason, path?, branch?}`: the repository could not be written: `ref_locked`
///   `{path}` (the branch's lock file is there; the hint removes it once no git process runs),
///   `ref_name_conflict` `{branch}` (a ref in the branch's way) or `io` `{path}`. The objects
///   written stay, loose, and the branch is as it was.
/// - `E_GIT_READ` `{reason, object?}`: an object of the repository that is missing or damaged, or
///   a read that failed (`io`).
/// - A store failure is the code its repository error has above (`E_BUSY` when the committed
///   snapshot's lock wait runs out, `E_IO`, `E_STORE_CORRUPT`, …), and a commit whose rendering
///   disagrees with its export oid is `E_INTERNAL`, a bug; either way the branch did not move.
pub struct LocalOps {
    pub(crate) repo: LocalRepo,
    /// The store's path as the caller gave it, for hints and display.
    store_path: Utf8PathBuf,
    /// The key the git export and import trust export oid memos under, if the caller gave one
    /// ([`LocalOps::with_memo_key`]).
    pub(crate) memo_key: Option<MemoKey>,
}

impl fmt::Debug for LocalOps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalOps")
            .field("store_path", &self.store_path)
            .finish_non_exhaustive()
    }
}

impl LocalOps {
    /// Creates a store at `path` for a repository named `name`, creating the directory if it is
    /// missing, and opens it (CLI `init`). `E_EXISTS` if a store is there already,
    /// `E_UNSUPPORTED_FS` for a path under `/mnt/`, `E_INVALID_ARG` for a name of more than 255
    /// bytes (`details.store_limit` `repo_name_bytes`), before anything is created. A store made
    /// whole whose reading back for the report fails is that read's error, saying the store is
    /// made (`details.created`).
    pub fn init(path: &Utf8Path, name: &str) -> Result<(LocalOps, RepoInfo), OpError> {
        Self::init_as(path, name, None)
    }

    /// [`LocalOps::init`] of a store of format `format` (`InitReq.format`): this build's default
    /// when `None`. A format this build does not create is `E_INVALID_ARG`
    /// (`details.format`, `details.formats` the ones it does), before anything is created.
    pub fn init_as(
        path: &Utf8Path,
        name: &str,
        format: Option<u32>,
    ) -> Result<(LocalOps, RepoInfo), OpError> {
        let format = init_format(format)?;
        let repo = LocalRepo::create_as(path, name, format)
            .map_err(|e| error::repo_error(e, During::Init(path)))?;
        let ops = Self::with_repo(repo, path);
        let info = ops
            .repo_info(InfoReq::default())
            .map_err(|e| crate::error::created_but_unread(path, e))?;
        Ok((ops, info))
    }

    /// Opens the store at `path`. `E_NOT_A_REPO` if there is none, or if what is there is not a
    /// store this build reads; `E_UNSUPPORTED_FS` if the path leads under `/mnt/`.
    pub fn open(path: &Utf8Path) -> Result<LocalOps, OpError> {
        let repo = LocalRepo::open(path).map_err(|e| error::repo_error(e, During::Open(path)))?;
        Ok(Self::with_repo(repo, path))
    }

    /// [`StoreOps::land`], `gate` asked before each of the land's attempts with its number,
    /// counted from 1: a refusal stops the land there, nothing of it recorded, and is its error
    /// as the gate gave it. `serve`'s land judges its caller's grant so (the M15 design §6).
    pub(crate) fn land_gated(
        &self,
        who: &Identity,
        req: LandReq,
        gate: impl FnMut(u32) -> Result<(), OpError>,
    ) -> Result<LandResp, OpError> {
        self.recording(&Resolved::default())
            .land_with(who, req, gate)
    }

    /// The operations over the store at `path`, its every filesystem call going through `fs`: a
    /// test's watched filesystem.
    #[cfg(test)]
    pub(crate) fn open_on(path: &Utf8Path, fs: impl swarmops_store::FsOps + 'static) -> LocalOps {
        let repo = LocalRepo::open_with_fs(path, std::sync::Arc::new(fs)).expect("open the store");
        Self::with_repo(repo, path)
    }

    /// The store's path, as given to [`LocalOps::init`] or [`LocalOps::open`].
    pub fn store_path(&self) -> &Utf8Path {
        &self.store_path
    }

    /// These operations, their git export and import (`git export`, `git import`, `push`,
    /// `fetch`, `sync`) and their deep verify given `key`, the store's memo key, which the caller
    /// keeps outside the store: an export oid memo whose MAC under it checks stops their walk of
    /// the history below it, and every export oid they derive is memoized under it (storage spec
    /// §5, §7). Without one, they derive every export oid from the history below it.
    #[must_use]
    pub fn with_memo_key(mut self, key: MemoKey) -> LocalOps {
        self.memo_key = Some(key);
        self
    }

    /// The signature of a changeset that `who` records: the name and email `<store>/authors.toml`
    /// gives `who`'s family (read now, every time: operations spec §5), else the agent name and
    /// `<instance>@<family>.swarmops.local`, and `secs`, a keyed commit's key's time, or else the
    /// current time ([`signing_time`]), in UTC. `E_STORE_CORRUPT` if `authors.toml` is.
    pub(crate) fn native_signature(
        &self,
        who: &Identity,
        secs: Option<i64>,
    ) -> Result<Signature, OpError> {
        let author = authors::author_of(self.store_path(), who.family())?;
        let (name, email) = match author {
            Some(author) => (
                author.name.unwrap_or_else(|| who.agent().to_owned()),
                author.email,
            ),
            None => (
                who.agent().to_owned(),
                format!("{}@{}.swarmops.local", who.instance(), who.family()),
            ),
        };

        // A UTC offset written +0000 always passes the signature's check.
        let secs = secs.unwrap_or_else(signing_time);
        Signature::new(name.into_bytes(), email.into_bytes(), secs, 0, false)
            .map_err(|e| error::error(ErrorCode::Internal, e.to_string()))
    }

    /// The operations of one request: each revision they resolve is recorded in `resolved`,
    /// and in no other request's.
    pub fn recording<'a>(&'a self, resolved: &'a Resolved) -> Recording<'a> {
        Recording {
            ops: self,
            resolved,
        }
    }

    /// The operations over `repo`, the store at `path`, with the derivation of export oids
    /// registered ([`swarmops_git::register`]), once. Neither `git export` nor `git import` reads
    /// its values: each derives the export oids it uses from the store's own objects.
    fn with_repo(mut repo: LocalRepo, path: &Utf8Path) -> LocalOps {
        // A repository just opened has only the built-in derivations, none named `git_oid`.
        swarmops_git::register(&mut repo).expect("a repository just opened has no git_oid");
        LocalOps {
            repo,
            store_path: path.to_owned(),
            memo_key: None,
        }
    }
}

/// Changeset `id`, which a revision has just named in `read`.
fn stored(read: &Snapshot<'_>, id: ChangesetId) -> Result<StoredChangeset, OpError> {
    read.changeset(id)
        .map_err(error::op)?
        .ok_or_else(|| error::op(RepoError::NotFound(id.to_string())))
}

/// Why `read_tree` has no directory at `dir` (the root for `None`) in changeset `cs`, which
/// `rev` named in `read`: a file is there, or nothing is.
fn no_directory(
    read: &Snapshot<'_>,
    cs: ChangesetId,
    rev: &Rev,
    dir: Option<&RepoPath>,
) -> OpError {
    let path = dir.map_or("", RepoPath::as_str);
    match dir.map(|dir| read.entry_at(cs, dir).map_err(error::op)) {
        Some(Ok(Some(_))) => error::error(
            ErrorCode::PathNotFound,
            format!("{path:?} is a file at {rev}, not a directory"),
        )
        .with_details(rev_and_path(rev, path))
        .with_hint(error::hint(format!(
            "swarmops cat {}",
            error::shell_word(&format!("{rev}:{path}"))
        ))),
        Some(Err(e)) => e,
        _ => path_not_found(rev, path),
    }
}

/// Why `read_file` found no file at `path` (`repo_path`) in changeset `cs`, which `rev` named in
/// `read`: a directory is there (`E_IS_DIR`), or nothing is (`E_PATH_NOT_FOUND`).
fn no_file(
    read: &Snapshot<'_>,
    cs: ChangesetId,
    rev: &Rev,
    path: &str,
    repo_path: &RepoPath,
) -> OpError {
    match read.entry_at(cs, repo_path) {
        Ok(Some(EntryTarget::Dir(_))) => error::error(
            ErrorCode::IsDir,
            format!("{path:?} is a directory at {rev}"),
        )
        .with_details(rev_and_path(rev, path))
        .with_hint(error::hint(format!(
            "swarmops ls {}",
            error::shell_word(&format!("{rev}:{path}"))
        ))),
        Ok(_) => path_not_found(rev, path),
        Err(e) => error::op(e),
    }
}

/// The part of a file that `read_file`'s `offset` and `lines` name. `E_INVALID_ARG` for both at
/// once (an `offset` of 0 is the default, not a choice), and for lines numbered from 0 or a range
/// that ends before it starts.
fn file_span(offset: u64, lines: Option<LineRange>) -> Result<FileSpan, OpError> {
    let Some(LineRange { first, last, .. }) = lines else {
        return Ok(FileSpan::Bytes { start: offset });
    };
    let range = || {
        Value::from(Map::from_iter([
            ("first".to_owned(), Value::from(first)),
            ("last".to_owned(), Value::from(last)),
        ]))
    };
    if offset != 0 {
        return Err(error::error(
            ErrorCode::InvalidArg,
            format!(
                "read_file takes offset or lines, not both: offset {offset}, lines {first}-{last}"
            ),
        )
        .with_details(error::details([
            ("offset", offset.into()),
            ("lines", range()),
        ]))
        .with_hint("pass offset to read from a byte, or lines to read a range of lines"));
    }
    if first == 0 || last < first {
        return Err(error::error(
            ErrorCode::InvalidArg,
            format!(
                "lines {first}-{last} is no range of lines: they are numbered from 1, and a range \
                 ends at or after its start"
            ),
        )
        .with_details(error::details([("lines", range())])));
    }
    Ok(FileSpan::Lines { first, last })
}

/// `E_INVALID_ARG` unless `depth`, a `read_tree` request's, lists 1 to [`MAX_TREE_DEPTH`] levels.
fn tree_depth(depth: u32) -> Result<(), OpError> {
    if (1..=MAX_TREE_DEPTH).contains(&depth) {
        return Ok(());
    }
    Err(error::error(
        ErrorCode::InvalidArg,
        format!("depth must be 1 to {MAX_TREE_DEPTH}, not {depth}"),
    )
    .with_details(error::details([("depth", depth.into())])))
}

/// The path a `read_tree` page resumes after: its `cursor`, the last entry's path on the page
/// before. `E_INVALID_ARG` for a cursor that is no repository path.
fn listing_cursor(cursor: &str) -> Result<RepoPath, OpError> {
    RepoPath::new(cursor).map_err(|e| {
        error::error(
            ErrorCode::InvalidArg,
            format!("{cursor:?} is not a listing cursor: {e}"),
        )
        .with_details(error::details([("cursor", cursor.into())]))
        .with_hint("pass the previous page's cursor as it was given, or none to start over")
    })
}

/// Where a log page resumes after `cursor`, the last id of the page before: that changeset's
/// first parent as `read` records it, or `None` at a root. `E_INVALID_ARG` for a cursor that is
/// not the full id of a changeset in the store.
fn after_cursor(read: &Snapshot<'_>, cursor: &str) -> Result<Option<ChangesetId>, OpError> {
    let bad = |why: &str| {
        error::error(
            ErrorCode::InvalidArg,
            format!("{cursor:?} is not a log cursor: {why}"),
        )
        .with_details(error::details([("cursor", cursor.into())]))
        .with_hint("pass the previous page's cursor as it was given, or none to start over")
    };
    let id = ObjectId::from_hex(cursor)
        .map(ChangesetId::new)
        .map_err(|_| bad("a cursor is a full changeset id"))?;
    let stored = read
        .changeset(id)
        .map_err(error::op)?
        .ok_or_else(|| bad("the store holds no such changeset"))?;
    Ok(stored.changeset.parents.first().copied())
}

/// The paths a `diff` looks at, from its request's `paths`: every path for none, else those at
/// or below one of them ([`prefix_path`]).
fn diff_filter(paths: &[String]) -> Result<PathFilter, OpError> {
    if paths.is_empty() {
        return Ok(PathFilter::all());
    }
    let prefixes = paths.iter().map(|path| prefix_path(path));
    Ok(PathFilter::under(prefixes.collect::<Result<_, _>>()?))
}

/// Where a `diff` page resumes: after `cursor`, the last path of the page before, in path
/// order. `E_INVALID_ARG` for a cursor that is not a repository path.
fn diff_cursor(cursor: &str) -> Result<RepoPath, OpError> {
    RepoPath::new(cursor).map_err(|_| {
        error::error(
            ErrorCode::InvalidArg,
            format!("{cursor:?} is not a diff cursor: a diff cursor is a repository path"),
        )
        .with_details(error::details([("cursor", cursor.into())]))
        .with_hint("pass the previous page's cursor as it was given, or none to start over")
    })
}

/// A `diff` page of `changes`, the changes after the request's cursor: at most `limit` files,
/// each as `format` says it; a `unified` page also within `max_bytes` bytes of its files as
/// JSON, its first file whatever its size, that file's hunks cut to fit.
fn diff_page(
    read: &Snapshot<'_>,
    changes: &[TreeChange],
    format: DiffFormat,
    context: u32,
    limit: usize,
    max_bytes: u64,
) -> Result<Vec<FileDiff>, OpError> {
    let mut page: Vec<FileDiff> = Vec::new();
    // The bytes of the page's files as the JSON array they print as: `[]` so far.
    let mut used = 2;
    for change in changes.iter().take(limit) {
        let mut file = file_diff(read, change, format, context)?;
        if format == DiffFormat::Unified {
            let comma = u64::from(!page.is_empty());
            let room = max_bytes.saturating_sub(used + comma);
            if json_len(|out| serde_json::to_writer(out, &file)) > room {
                if !page.is_empty() {
                    // The page stops before the first file that would take it past `max_bytes`.
                    break;
                }
                cut_hunks(&mut file, room);
            }
            used += comma + json_len(|out| serde_json::to_writer(out, &file));
        }
        page.push(file);
    }
    Ok(page)
}

/// What a `diff` page says of `change` in `format`: its path and status; for `stat` and
/// `unified`, read from the blobs of its two sides (a blob both share read once), whether it is
/// binary and, for text, the lines it inserts and deletes; for `unified`, also its kinds, its
/// blobs and, for text, its hunks with `context` unchanged lines around each change. Text past
/// the line diff's bound gets neither line counts nor hunks: they are cut (`hunks_truncated`).
fn file_diff(
    read: &Snapshot<'_>,
    change: &TreeChange,
    format: DiffFormat,
    context: u32,
) -> Result<FileDiff, OpError> {
    let (before, after) = (change.kind.before(), change.kind.after());
    let mut file = FileDiff::new(change.path.to_string(), diff_status(change.kind)?);
    if format == DiffFormat::Names {
        return Ok(file);
    }
    if format == DiffFormat::Unified {
        file.old_kind = before.map(|leaf| file_kind(leaf.kind)).transpose()?;
        file.new_kind = after.map(|leaf| file_kind(leaf.kind)).transpose()?;
        file.old_blob = before.map(|leaf| leaf.blob.to_string());
        file.new_blob = after.map(|leaf| leaf.blob.to_string());
    }
    // Each side's bytes, none where the file is not. Two sides that share a blob differ in the
    // exec bit or the kind only.
    let shared = before
        .zip(after)
        .is_some_and(|(old, new)| old.blob == new.blob);
    if !shared && let Some(binary) = past_the_bound(read, before, after)? {
        file.binary = Some(binary);
        if !binary {
            // Past the line diff's bound (`TooLarge`): no line counts, and no hunk, cut.
            file.hunks_truncated = Some(true);
            file.hunks = (format == DiffFormat::Unified).then(Vec::new);
        }
        return Ok(file);
    }
    let old = before
        .map(|leaf| read.blob(leaf.blob))
        .transpose()
        .map_err(error::op)?;
    let new = match after {
        Some(leaf) if !shared => Some(read.blob(leaf.blob).map_err(error::op)?),
        _ => None,
    };
    let old = old.as_deref().unwrap_or_default();
    let new = if shared {
        old
    } else {
        new.as_deref().unwrap_or_default()
    };
    let text = line_diff::is_text(old) && line_diff::is_text(new);
    file.binary = Some(!text);
    if !text {
        return Ok(file);
    }
    let hunks = if shared {
        Vec::new()
    } else if let Ok(hunks) = line_diff::line_hunks(old, new) {
        hunks
    } else {
        // A side past the line diff's bound (`TooLarge`): no line counts, and no hunk, cut.
        file.hunks_truncated = Some(true);
        file.hunks = (format == DiffFormat::Unified).then(Vec::new);
        return Ok(file);
    };
    let stats = line_diff::line_stats(&hunks);
    file.insertions = Some(stats.insertions);
    file.deletions = Some(stats.deletions);
    if format == DiffFormat::Unified {
        let shown = line_diff::unified(old, new, &hunks, context);
        file.hunks = Some(shown.iter().map(diff_hunk).collect::<Result<_, _>>()?);
    }
    Ok(file)
}

/// For two sides that do not share a blob, whether the file is binary, decided without reading a
/// blob longer than the line diff's bound ([`line_diff::MAX_DIFF_BYTES`], its length from the blob's
/// header): `Some(true)` if a side within the bound is not text, `Some(false)` if one is past the
/// bound and the others are text, which a line diff would have refused (`TooLarge`). A side past
/// the bound is taken for text: its bytes are not read to tell. `None` if no side is past the
/// bound, so that both are read.
fn past_the_bound(
    read: &Snapshot<'_>,
    before: Option<Leaf>,
    after: Option<Leaf>,
) -> Result<Option<bool>, OpError> {
    let limit = bytes::to_u64(line_diff::MAX_DIFF_BYTES);
    let mut small = Vec::new();
    let mut past = false;
    for leaf in [before, after].into_iter().flatten() {
        if read.blob_len(leaf.blob).map_err(error::op)? > limit {
            past = true;
        } else {
            small.push(leaf.blob);
        }
    }
    if !past {
        return Ok(None);
    }
    for blob in small {
        let bytes = read.blob(blob).map_err(error::op)?;
        if !line_diff::is_text(&bytes) {
            return Ok(Some(true));
        }
    }
    Ok(Some(false))
}

/// What happened to a file, as `diff` names it. `E_INTERNAL` for a change this build does not
/// know.
fn diff_status(kind: ChangeKind) -> Result<DiffStatus, OpError> {
    match kind {
        ChangeKind::Added { .. } => Ok(DiffStatus::Added),
        ChangeKind::Deleted { .. } => Ok(DiffStatus::Deleted),
        ChangeKind::Modified { .. } => Ok(DiffStatus::Modified),
        ChangeKind::TypeChanged { .. } => Ok(DiffStatus::TypeChanged),
        _ => Err(error::unknown_variant("ChangeKind")),
    }
}

/// `hunk` as a `unified` page gives it. Its lines are UTF-8: a file gets hunks only when both
/// its sides are text, and a line ends after a `\n`, a byte no other character holds.
/// `E_INTERNAL` for a line this build does not know.
fn diff_hunk(hunk: &UnifiedHunk<'_>) -> Result<DiffHunk, OpError> {
    let lines = (hunk.lines.iter())
        .map(|line| {
            let (op, text) = match *line {
                DiffLine::Context(text) => (LineOp::Context, text),
                DiffLine::Removed(text) => (LineOp::Remove, text),
                DiffLine::Added(text) => (LineOp::Add, text),
                _ => return Err(error::unknown_variant("DiffLine")),
            };
            Ok(DiffLineOut::new(op, lossy(text)))
        })
        .collect::<Result<_, _>>()?;
    Ok(DiffHunk::new(
        u64::from(hunk.before_start),
        u64::from(hunk.before_lines),
        u64::from(hunk.after_start),
        u64::from(hunk.after_lines),
        lines,
    ))
}

/// Cuts `file`'s hunks to the first ones that fit with it in `room` bytes of JSON, and marks it
/// cut (`hunks_truncated`); with room for none, it keeps none. A file without hunks to cut (a
/// binary one, one past the line diff's bound, or one whose content did not change) stays as it
/// is.
fn cut_hunks(file: &mut FileDiff, room: u64) {
    let hunks = match file.hunks.take() {
        Some(hunks) if !hunks.is_empty() => hunks,
        none => {
            file.hunks = none;
            return;
        }
    };
    file.hunks = Some(Vec::new());
    file.hunks_truncated = Some(true);
    let mut size = json_len(|out| serde_json::to_writer(out, &*file));
    let mut kept = Vec::new();
    for hunk in hunks {
        let more = json_len(|out| serde_json::to_writer(out, &hunk)) + u64::from(!kept.is_empty());
        if size + more > room {
            break;
        }
        size += more;
        kept.push(hunk);
    }
    file.hunks = Some(kept);
}

/// How many bytes `write`, a serialization of an API value, writes: the value's length as the
/// envelope prints it (compact JSON).
fn json_len(write: impl FnOnce(&mut ByteCount) -> serde_json::Result<()>) -> u64 {
    let mut count = ByteCount(0);
    // An API value always serializes: its maps have string keys, its impls never fail.
    write(&mut count).expect("an API value serializes");
    count.0
}

/// A writer that keeps nothing but the number of bytes written to it.
struct ByteCount(u64);

impl io::Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes::to_u64(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The revisions one request resolved: each as given, and the full changeset id it named. The
/// CLI and the MCP server report it as `meta.resolved`. And what the request's write repaired
/// before its own work, if it repaired anything (`commit`, `ref_set`, `ref_delete` and `undo`
/// record it, on success and on `E_CAS_MISMATCH` alike), which they report as `meta.recovered`.
///
/// Each request has its own: [`LocalOps::recording`] records into it what that request's
/// operations resolve and repair, and nothing else does, so that what one request resolved or
/// repaired never shows in another's, also when the two share a `LocalOps` across threads.
#[derive(Debug, Default)]
pub struct Resolved {
    revs: RefCell<BTreeMap<String, String>>,
    recovered: Cell<Option<Recovered>>,
}

impl Resolved {
    /// What has been recorded, each revision as given and the id it named; taking it empties
    /// the record.
    pub fn take(&self) -> BTreeMap<String, String> {
        self.revs.take()
    }

    /// What the request's write repaired before its own work, if it repaired anything; taking
    /// it empties the record.
    pub fn take_recovered(&self) -> Option<RecoveredOut> {
        self.recovered.take().map(recovered_out)
    }

    /// Records that the revision `rev` named the changeset `id`.
    pub(crate) fn record(&self, rev: &str, id: ChangesetId) {
        self.revs
            .borrow_mut()
            .insert(rev.to_owned(), id.to_string());
    }

    /// Records `recovered`, what the request's write repaired, unless it repaired nothing.
    pub(crate) fn record_recovered(&self, recovered: Recovered) {
        if !recovered.is_empty() {
            self.recovered.set(Some(recovered));
        }
    }
}

/// `recovered` as the envelope's `meta.recovered` gives it.
fn recovered_out(recovered: Recovered) -> RecoveredOut {
    let Recovered {
        log_cut,
        frames_listed,
        frames_rewritten,
        off_bytes_dropped,
        index_records_dropped,
        views_rebuilt,
        records_replayed,
        refs_replayed,
        ..
    } = recovered;
    let mut out = RecoveredOut::default();
    out.log_cut = log_cut.map(|LogCut { at, bytes, .. }| LogCutOut::new(at, bytes));
    out.frames_listed = frames_listed;
    out.frames_rewritten = frames_rewritten;
    out.off_bytes_dropped = off_bytes_dropped;
    out.index_records_dropped = index_records_dropped;
    out.views_rebuilt = views_rebuilt;
    out.records_replayed = records_replayed;
    out.refs_replayed = refs_replayed;
    out
}

/// [`LocalOps`]'s operations as one request: [`StoreOps`], recording each revision they resolve
/// in the request's [`Resolved`] ([`LocalOps::recording`]).
#[derive(Debug, Clone, Copy)]
pub struct Recording<'a> {
    pub(crate) ops: &'a LocalOps,
    pub(crate) resolved: &'a Resolved,
}

impl<'a> Recording<'a> {
    /// A snapshot of the store's metadata: the one in which the operation, taking it at its
    /// start, makes all the reads of its request.
    fn snapshot(&self) -> Result<Snapshot<'a>, OpError> {
        self.ops.repo.snapshot().map_err(error::op)
    }

    /// The changeset `rev` names in `read`, recorded as resolved.
    fn resolve(&self, read: &Snapshot<'_>, rev: &Rev) -> Result<ChangesetId, OpError> {
        let id = read.resolve(rev.as_str()).map_err(error::op)?;
        self.resolved.record(rev.as_str(), id);
        Ok(id)
    }

    /// What `main` names in `read`, if it exists, recorded as resolved: what an operation uses
    /// when its request names no revision.
    fn main_head(&self, read: &Snapshot<'_>) -> Result<Option<ChangesetId>, OpError> {
        self.head(read, "main")
    }

    /// What ref `name` names in `read`, if it exists, recorded as resolved under the ref's name:
    /// `main`'s head for an operation whose request names no revision, the advanced ref's for a
    /// commit with `advance` and no parent.
    fn head(&self, read: &Snapshot<'_>, name: &str) -> Result<Option<ChangesetId>, OpError> {
        let head = read
            .get_ref(name)
            .map_err(error::op)?
            .map(|state| state.target);
        if let Some(id) = head {
            self.resolved.record(name, id);
        }
        Ok(head)
    }

    /// A land's or a rebase's change, resolved in one snapshot: its `head`, its `base`, the
    /// changeset `expected` names (a land's `expect_target`), and the `against` of its
    /// resolutions; the head of ref `onto` is recorded as it names it. The resolutions' contents
    /// are read once the revisions resolved, as a commit's are. The land and the rebase read the
    /// ref again in their own snapshots.
    fn change(
        &self,
        head: &Rev,
        base: Option<&Rev>,
        expected: Option<&Rev>,
        onto: &str,
        resolutions: Option<ShapedResolutions>,
    ) -> Result<ResolvedChange, OpError> {
        // 1. The revisions.
        let read = self.snapshot()?;
        let head = self.resolve(&read, head)?;
        let resolve = |rev: Option<&Rev>| rev.map(|rev| self.resolve(&read, rev)).transpose();
        let base = resolve(base)?;
        let expected = resolve(expected)?;
        let against = resolve(resolutions.as_ref().map(|shaped| &shaped.against))?;
        self.head(&read, onto)?;
        drop(read);

        // 2. The resolutions' contents.
        let resolutions = match (resolutions, against) {
            (Some(shaped), Some(against)) => Some(shaped.resolutions(against)?),
            _ => None,
        };
        Ok(ResolvedChange {
            head,
            base,
            expected,
            resolutions,
        })
    }

    /// `rebase` (M5) of `req` as `who`, its ref name checked ([`crate::rebase`]): a keyed
    /// rebase's key is a uuid v7, as a commit's, and it signs at the key's time; the recorded
    /// changeset's metadata is a commit's (`agent`, `commit_key`, `run_id`).
    pub(crate) fn rebase_change(
        &self,
        who: &Identity,
        req: RebaseReq,
    ) -> Result<swarmops_repo::RebaseOutcome, OpError> {
        let RebaseReq {
            head,
            base,
            onto,
            message,
            idempotency_key,
            max_conflict_bytes,
            resolutions,
            ..
        } = req;

        // 1. The request's own rules, before anything is resolved or read.
        let key = idempotency_key
            .as_deref()
            .map(CommitKey::parse)
            .transpose()?;
        let resolutions = resolutions.map(shaped_resolutions).transpose()?;

        // 2. The change, resolved in one snapshot.
        let ResolvedChange {
            head,
            base,
            resolutions,
            ..
        } = self.change(&head, base.as_ref(), None, &onto, resolutions)?;

        // 3. Its metadata and signature, as a commit's.
        let signature = self
            .ops
            .native_signature(who, key.as_ref().map(|key| key.secs))?;
        let mut metadata = vec![("agent".to_owned(), who.agent().as_bytes().to_vec())];
        if let Some(key) = &key {
            metadata.push((COMMIT_KEY.to_owned(), key.text.clone().into_bytes()));
        }
        if let Some(run_id) = who.run_id() {
            metadata.push(("run_id".to_owned(), run_id.as_bytes().to_vec()));
        }
        content::check_metadata(&metadata)?;

        // 4. The rebase.
        let mut rebase = swarmops_repo::RebaseRequest::new(head, &onto, signature);
        rebase.base = base;
        rebase.message = message.map(String::into_bytes);
        rebase.metadata = metadata;
        rebase.resolutions = resolutions;
        let outcome = (self.ops.repo)
            .rebase(rebase)
            .map_err(|e| merge_error(e, max_conflict_bytes))?;
        self.resolved.record_recovered(outcome.recovered);
        Ok(outcome)
    }

    /// The replay of a commit keyed `key` whose parent, the changeset it would build on, is
    /// `parent`, when that is the changeset an earlier request with the key made (its
    /// `commit_key`, read in `read`, the snapshot `parent` was resolved in; one object read): the
    /// commit is made, and making it again would apply its change a second time. `None`
    /// otherwise.
    ///
    /// The replay is decided under the store's lock, after the write's tail check, as a ref
    /// move's replay is, never from this request's snapshot, which shows a whole frame that its
    /// writer may not have synced yet: by a move of the ref the commit advances (`main` for one
    /// that advances none) to the target it names, which [`Repo::cas_ref`] compares there and
    /// never records. That move finds `parent` gone where the tail check took it back; then there
    /// is no replay, and the commit goes on, as one on a parent that is gone does. With
    /// `advance`, the ref must name `parent`, as `already_applied`; a ref that names another is
    /// `E_CAS_MISMATCH`, which the commit itself would be. Either way the write's repairs are
    /// recorded, as a commit's are.
    ///
    /// [`Repo::cas_ref`]: swarmops_repo::Repo::cas_ref
    fn replay_of_key(
        &self,
        read: &Snapshot<'_>,
        who: &Identity,
        key: &CommitKey,
        parent: ChangesetId,
        advance: Option<&str>,
    ) -> Result<Option<CommitResp>, OpError> {
        let StoredChangeset { changeset, .. } = stored(read, parent)?;
        if !key.made(&changeset.metadata) {
            return Ok(None);
        }
        let name = advance.unwrap_or("main");
        let confirmed = match (self.ops.repo).cas_ref(name, Some(parent), Some(parent), who.agent())
        {
            Ok(confirmed) => confirmed,
            Err(RepoError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(error::op(e)),
        };
        self.resolved.record_recovered(confirmed.recovered);
        let old = changeset.parents.first().copied();
        let advanced = match (advance, confirmed.result) {
            (None, _) => None,
            (Some(name), CasResult::Applied { .. }) => {
                let mut moved = swarmops_api::Advanced::new(name, parent.to_string(), true);
                moved.old = old.map(|id| id.to_string());
                Some(moved)
            }
            (Some(name), CasResult::Mismatch { actual }) => {
                return Err(error::op(RepoError::RefMoved {
                    name: name.to_owned(),
                    actual,
                    recovered: Box::new(confirmed.recovered),
                }));
            }
            (Some(_), _) => return Err(error::unknown_variant("CasResult")),
        };
        let mut out = CommitResp::new(
            parent.to_string(),
            changeset.root.to_string(),
            bytes::to_u64(changeset.delta.len()),
            0,
        );
        out.parent = old.map(|id| id.to_string());
        out.advanced = advanced;
        out.warnings = confirmed.warnings;
        out.replayed = true;
        Ok(Some(out))
    }

    /// [`StoreOps::land`], `gate` asked before each of the land's attempts with its number,
    /// counted from 1 ([`swarmops_repo::Repo::land_gated`]): a refusal is the land's error, as
    /// the gate gave it.
    fn land_with(
        &self,
        who: &Identity,
        req: LandReq,
        gate: impl FnMut(u32) -> Result<(), OpError>,
    ) -> Result<LandResp, OpError> {
        let LandReq {
            head,
            base,
            target,
            message,
            idempotency_key,
            max_attempts,
            max_conflict_bytes,
            expect_target,
            resolutions,
            ..
        } = req;
        // The request's own rules are checked before anything is resolved or read.
        check_ref_use(&target, RefUse::Publish)?;
        if let Some(key) = &idempotency_key {
            land_key(key)?;
        }
        if !(1..=MAX_LAND_ATTEMPTS).contains(&max_attempts) {
            return Err(error::error(
                ErrorCode::InvalidArg,
                format!("max_attempts is {max_attempts}, not 1 to {MAX_LAND_ATTEMPTS}"),
            )
            .with_details(error::details([("max_attempts", max_attempts.into())])));
        }
        let resolutions = resolutions.map(shaped_resolutions).transpose()?;

        // 1. The change and the head it expects the target to name, resolved in one snapshot.
        let ResolvedChange {
            head,
            base,
            expected,
            resolutions,
        } = self.change(
            &head,
            base.as_ref(),
            expect_target.as_ref(),
            &target,
            resolutions,
        )?;

        // 2. The land itself.
        let mut lander_metadata = Vec::new();
        if let Some(run_id) = who.run_id() {
            let run_id = run_id.as_bytes().to_vec();
            lander_metadata.push((LANDER_RUN_ID_KEY.to_owned(), run_id));
        }
        let mut land = LandRequest::new(head, &target, self.ops.native_signature(who, None)?);
        land.base = base;
        land.key = idempotency_key;
        land.message = message.map(String::into_bytes);
        land.lander_metadata = lander_metadata;
        land.max_attempts = max_attempts;
        land.expect_target = expected;
        land.resolutions = resolutions;
        let outcome = match self.ops.repo.land_gated(land, gate) {
            Ok(outcome) => outcome,
            // The gate's refusal, as it gave it.
            Err(Gated::Refused(e)) => return Err(e),
            Err(Gated::Repo(e)) => {
                // An exact land refused under the lock reports what the write repaired first.
                if let RepoError::CasMismatch { recovered, .. } = &e {
                    self.resolved.record_recovered(**recovered);
                }
                return Err(merge_error(e, max_conflict_bytes));
            }
            Err(_) => return Err(error::unknown_variant("Gated")),
        };
        let status = match outcome.status {
            swarmops_repo::LandStatus::Landed => LandStatus::Landed,
            swarmops_repo::LandStatus::Replayed => LandStatus::Replayed,
            _ => return Err(error::unknown_variant("LandStatus")),
        };
        let mut out = LandResp::new(
            status,
            outcome.landed.to_string(),
            outcome.rebased_onto.to_string(),
            target,
            outcome.base.to_string(),
            outcome.attempts,
            outcome.relocated,
            bytes::to_u64(outcome.files_changed),
            (outcome.squashed_from.iter())
                .map(ToString::to_string)
                .collect(),
        );
        out.auto_merged = (outcome.auto_merged.iter())
            .map(ToString::to_string)
            .collect();
        out.warnings = outcome.warnings;
        Ok(out)
    }
}

impl StoreOps for Recording<'_> {
    fn repo_info(&self, _req: InfoReq) -> Result<RepoInfo, OpError> {
        let info = self.ops.repo.info().map_err(error::op)?;
        let mut out = RepoInfo::new(
            info.repo_id,
            info.name,
            info.store_version,
            info.changesets,
            info.refs,
        );
        out.main = info.main.map(|id| id.to_string());
        Ok(out)
    }

    fn commit(&self, who: &Identity, req: CommitReq) -> Result<CommitResp, OpError> {
        let CommitReq {
            parent,
            changes,
            message,
            advance,
            idempotency_key,
            allow_empty,
            ..
        } = req;
        // A ref a commit cannot publish on, and a key that is not a uuid v7, are refused before
        // anything is resolved or read.
        if let Some(name) = &advance {
            check_ref_use(name, RefUse::Publish)?;
        }
        let key = idempotency_key
            .as_deref()
            .map(CommitKey::parse)
            .transpose()?;
        let read = self.snapshot()?;
        let parent = match (&parent, &advance) {
            (Some(rev), _) => Some(self.resolve(&read, rev)?),
            // A commit that advances a ref builds on that ref's head by default.
            (None, Some(name)) => self.head(&read, name)?,
            (None, None) => self.main_head(&read)?,
        };
        // A keyed commit whose parent is the changeset its key made has been made: it replays
        // that changeset rather than make its change again on top of it.
        if let (Some(key), Some(on)) = (&key, parent)
            && let Some(replay) = self.replay_of_key(&read, who, key, on, advance.as_deref())?
        {
            return Ok(replay);
        }
        let changes = content::changes(changes)?;
        // Where each blob named by id is in the list, to say which change names one the store
        // lacks.
        let named: BTreeMap<RepoPath, usize> = (changes.iter().enumerate())
            .filter(|(_, change)| matches!(change.op, swarmops_repo::ChangeOp::PutBlob { .. }))
            .map(|(index, change)| (change.path.clone(), index))
            .collect();
        let signature = self
            .ops
            .native_signature(who, key.as_ref().map(|key| key.secs))?;
        // In ascending key order, as a changeset's metadata must be.
        let mut metadata = vec![("agent".to_owned(), who.agent().as_bytes().to_vec())];
        if let Some(key) = &key {
            metadata.push((COMMIT_KEY.to_owned(), key.text.clone().into_bytes()));
        }
        if let Some(run_id) = who.run_id() {
            metadata.push(("run_id".to_owned(), run_id.as_bytes().to_vec()));
        }
        content::check_metadata(&metadata)?;
        let advance = advance.map(|name| swarmops_repo::Advance::new(&name, who.agent()));
        let request = CommitRequest::new(
            parent.into_iter().collect(),
            changes,
            signature.clone(),
            signature,
            message.into_bytes(),
            metadata,
            allow_empty,
            advance,
        );
        // The parent is checked in the snapshot it was resolved in, which is not read after the
        // commit.
        let outcome = self.ops.repo.commit_in(&read, request).map_err(|e| {
            // A move refused under the lock is E_CAS_MISMATCH, which reports what the write
            // repaired first, as a ref move's does.
            if let RepoError::RefMoved { recovered, .. } = &e {
                self.resolved.record_recovered(**recovered);
            }
            // A blob the list names that the store lacks is said of its change, as the list's
            // other refusals are.
            let at = match &e {
                RepoError::MissingBlob { path, .. } => named
                    .get(path)
                    .map(|&index| (index, path.as_str().to_owned())),
                _ => None,
            };
            match at {
                Some((index, path)) => content::in_change(error::op(e), index, &path),
                None => error::op(e),
            }
        })?;
        self.resolved.record_recovered(outcome.recovered);
        // The store held the changeset already, and the ref it advances, if any, named it: an
        // identical request made both, and this one wrote nothing.
        let replayed = matches!(outcome.insert, swarmops_repo::InsertOutcome::AlreadyPresent)
            && (outcome.advanced.as_ref()).is_none_or(|moved| moved.already_applied);
        let mut out = CommitResp::new(
            outcome.id.to_string(),
            outcome.root.to_string(),
            bytes::to_u64(outcome.files_changed),
            bytes::to_u64(outcome.blobs_written),
        );
        out.parent = parent.map(|id| id.to_string());
        out.advanced = outcome.advanced.map(|moved| {
            let mut advanced = swarmops_api::Advanced::new(
                moved.name,
                outcome.id.to_string(),
                moved.already_applied,
            );
            advanced.old = moved.old.map(|id| id.to_string());
            advanced
        });
        out.warnings = outcome.warnings;
        out.replayed = replayed;
        Ok(out)
    }

    fn land(&self, who: &Identity, req: LandReq) -> Result<LandResp, OpError> {
        // No gate: every attempt goes ahead.
        self.land_with(who, req, |_| Ok(()))
    }

    fn blob_put(&self, _who: &Identity, req: BlobPutReq) -> Result<BlobPutResp, OpError> {
        let BlobPutReq { content, .. } = req;
        let stored = (self.ops.repo)
            .put_blob(content::bytes(content)?)
            .map_err(error::op)?;
        Ok(BlobPutResp::new(
            stored.id.to_string(),
            stored.size,
            stored.written,
        ))
    }

    fn rebase(&self, who: &Identity, req: RebaseReq) -> Result<RebaseResp, OpError> {
        crate::rebase::run(self, who, req)
    }

    fn blob_get(&self, req: BlobGetReq) -> Result<BlobGetResp, OpError> {
        crate::blob_get::run(self, req)
    }

    fn blob_has(&self, req: BlobHasReq) -> Result<BlobHasResp, OpError> {
        // Every id is checked before the store is asked about any.
        let ids = (req.ids.iter().enumerate())
            .map(|(index, id)| {
                content::blob_id(id).map_err(|mut e| {
                    e.message = format!("id {index}: {}", e.message);
                    e.details.insert("index".to_owned(), index.into());
                    e
                })
            })
            .collect::<Result<Vec<_>, OpError>>()?;
        let mut asked = BTreeSet::new();
        let mut present = Vec::new();
        for id in ids {
            if asked.insert(id) && self.ops.repo.has_blob(id).map_err(error::op)? {
                present.push(id.to_string());
            }
        }
        Ok(BlobHasResp::new(present))
    }

    fn resolve_rev(&self, req: ResolveRevReq) -> Result<ResolvedRev, OpError> {
        let ResolveRevReq { rev, .. } = req;
        let read = self.snapshot()?;
        let id = self.resolve(&read, &rev)?;
        // The revision is a ref's name when the snapshot holds a ref by that very name, which is
        // then what it resolved through; an id, a prefix, or anything with a `~N` is no ref name,
        // and the lookup refuses it.
        let kind = match read.get_ref(rev.as_str()) {
            Ok(Some(_)) => RevKind::Ref,
            Ok(None)
            | Err(RepoError::RefName(_) | RepoError::Store(StoreError::FormatRequired { .. })) => {
                RevKind::Changeset
            }
            Err(e) => return Err(error::op(e)),
        };
        Ok(ResolvedRev::new(id.to_string(), kind))
    }

    fn read_file(&self, req: ReadFileReq) -> Result<FileContent, OpError> {
        let ReadFileReq {
            rev,
            path,
            offset,
            lines,
            max_bytes,
            ..
        } = req;
        // A cap of 0 would return nothing, truncated, with a `next_offset` where it started.
        at_least_one_byte(max_bytes)?;
        let span = file_span(offset, lines)?;
        let read = self.snapshot()?;
        let cs = self.resolve(&read, &rev)?;
        let repo_path = content::repo_path(&path)?;
        // Up to CHAR_TAIL bytes past the cut, for `encode` to see whether they complete a
        // character it cuts.
        let past_the_cut = max_bytes.saturating_add(content::CHAR_TAIL);
        let Some(got) =
            (read.read_file_span(cs, &repo_path, span, past_the_cut)).map_err(error::op)?
        else {
            return Err(no_file(&read, cs, &rev, &path, &repo_path));
        };
        let keep =
            usize::try_from(max_bytes).map_or(got.bytes.len(), |max| max.min(got.bytes.len()));
        let encoded = content::encode(&got.bytes, keep);
        // The span runs on past the bytes returned exactly when it is longer than max_bytes.
        let truncated = keep < got.bytes.len();
        let mut out = FileContent::new(
            got.blob.to_string(),
            got.size,
            file_kind(got.kind)?,
            encoded.encoding,
            encoded.content,
            truncated,
        );
        out.offset = (got.start > 0).then_some(got.start);
        out.lines = got.lines.map(|(first, last)| LineRange::new(first, last));
        out.next_offset = truncated.then(|| got.start + bytes::to_u64(encoded.len));
        Ok(out)
    }

    fn read_tree(&self, req: ReadTreeReq) -> Result<TreeListing, OpError> {
        let ReadTreeReq {
            rev,
            path,
            depth,
            limit,
            cursor,
            ..
        } = req;
        at_least_one("limit", limit)?;
        tree_depth(depth)?;
        let after = cursor.as_deref().map(listing_cursor).transpose()?;
        let read = self.snapshot()?;
        let cs = self.resolve(&read, &rev)?;
        // One trailing `/` is taken (`ls main:src/`). A path malformed even without it is
        // E_INVALID_ARG naming it as given; past that, an error (no directory there) names the
        // path without the `/`.
        let given = path
            .as_deref()
            .map(|path| (path, path.strip_suffix('/').unwrap_or(path)));
        let dir = match given {
            None | Some((_, "")) => None,
            Some((path, trimmed)) => {
                Some(RepoPath::new(trimmed).or_else(|_| content::repo_path(path))?)
            }
        };
        let listed = read.list_tree_depth(cs, dir.as_ref(), depth, after.as_ref(), limit);
        let Some(page) = listed.map_err(error::op)? else {
            return Err(no_directory(&read, cs, &rev, dir.as_ref()));
        };
        let entries = (page.entries.iter())
            .map(|entry| tree_entry_out(entry, EntryPath::when_deep(depth)))
            .collect::<Result<_, _>>()?;
        let mut out = TreeListing::new(entries, page.truncated);
        out.cursor = (page.entries.last())
            .filter(|_| page.truncated)
            .map(|last| last.path.to_string());
        Ok(out)
    }

    fn show(&self, req: ShowReq) -> Result<ChangesetDetail, OpError> {
        let ShowReq {
            rev, max_changes, ..
        } = req;
        at_least_one("max_changes", max_changes)?;
        let read = self.snapshot()?;
        let StoredChangeset {
            id, seq, changeset, ..
        } = stored(&read, self.resolve(&read, &rev)?)?;
        let changes = (changeset.delta.iter())
            .take(max_changes)
            .map(change_out)
            .collect::<Result<_, _>>()?;
        let metadata = (changeset.metadata.iter())
            .map(|(key, value)| content::metadata_entry(key, value))
            .collect();
        let signatures = Signatures::new(
            signature_out(&changeset.author),
            signature_out(&changeset.committer),
        );
        let mut out = ChangesetDetail::new(
            id.to_string(),
            seq,
            changeset.parents.iter().map(ToString::to_string).collect(),
            changeset.root.to_string(),
            attribution(&changeset.metadata),
            created_at(&changeset.committer),
            lossy(&changeset.message),
            changes,
            bytes::to_u64(changeset.delta.len()),
            changeset.delta.len() > max_changes,
            metadata,
            signatures,
        );
        out.lander = lander(&changeset.metadata);
        out.land_key = metadata_text(&changeset.metadata, LAND_KEY_KEY);
        out.squashed_from = (metadata_text(&changeset.metadata, SQUASHED_FROM_KEY))
            .map(|chain| chain.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();
        Ok(out)
    }

    fn log(&self, req: LogReq) -> Result<LogPage, OpError> {
        let LogReq {
            rev,
            limit,
            cursor,
            paths,
            ..
        } = req;
        at_least_one("limit", limit)?;
        let filter = log_filter(&paths)?;
        let read = self.snapshot()?;
        let from = match cursor {
            Some(cursor) => match after_cursor(&read, &cursor)? {
                Some(parent) => parent,
                None => {
                    return Ok(LogPage::new(Vec::new(), false));
                }
            },
            None => self.resolve(&read, &rev.unwrap_or_else(|| Rev::from("main")))?,
        };
        if let Some(filter) = filter {
            // One changeset more than the page: whether another that changes the paths follows.
            let mut changesets = read
                .log_paths(from, &filter, limit.saturating_add(1))
                .map_err(error::op)?;
            let truncated = changesets.len() > limit;
            changesets.truncate(limit);
            let mut out = LogPage::new(changesets.iter().map(summary).collect(), truncated);
            out.cursor = (changesets.last())
                .filter(|_| truncated)
                .map(|last| last.id.to_string());
            return Ok(out);
        }
        let changesets = read.log_first_parent(from, limit).map_err(error::op)?;
        // A full page whose last changeset has a parent has older history after it.
        let last = changesets
            .last()
            .filter(|last| changesets.len() == limit && !last.changeset.parents.is_empty());
        let mut out = LogPage::new(changesets.iter().map(summary).collect(), last.is_some());
        out.cursor = last.map(|last| last.id.to_string());
        Ok(out)
    }

    fn path_history(&self, req: PathHistoryReq) -> Result<PathHistoryPage, OpError> {
        let PathHistoryReq {
            rev,
            path,
            limit,
            cursor,
            ..
        } = req;
        at_least_one("limit", limit)?;
        let path = prefix_path(&path)?;
        let read = self.snapshot()?;
        let rev = rev.unwrap_or_else(|| Rev::from("main"));
        let from = match &cursor {
            Some(cursor) => after_cursor(&read, cursor)?,
            None => Some(self.resolve(&read, &rev)?),
        };
        // One event more than the page: whether an older one follows.
        let mut events = match from {
            Some(from) => read
                .path_history(from, &path, limit.saturating_add(1))
                .map_err(error::op)?,
            None => Vec::new(),
        };
        if events.is_empty() && cursor.is_none() {
            return Err(no_history(&rev, path.as_str()));
        }
        let truncated = events.len() > limit;
        events.truncate(limit);
        let entries = (events.iter())
            .map(|event| history_entry(&read, event))
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = PathHistoryPage::new(path.to_string(), entries, truncated);
        out.cursor = (events.last())
            .filter(|_| truncated)
            .map(|last| last.changeset.to_string());
        Ok(out)
    }

    fn diff(&self, req: DiffReq) -> Result<Diff, OpError> {
        let DiffReq {
            from,
            to,
            paths,
            format,
            context,
            limit,
            cursor,
            max_bytes,
            ..
        } = req;
        at_least_one("limit", limit)?;
        at_least_one_byte(max_bytes)?;
        let filter = diff_filter(&paths)?;
        let after = cursor.as_deref().map(diff_cursor).transpose()?;
        // Both revisions resolve in one snapshot, which every read of the diff is made in.
        let read = self.snapshot()?;
        let (old, new) = (self.resolve(&read, &from)?, self.resolve(&read, &to)?);
        let changes = read.diff(old, new, &filter).map_err(error::op)?;
        let start = after.map_or(0, |after| {
            changes.partition_point(|change| change.path <= after)
        });
        let rest = &changes[start..];
        let files = diff_page(&read, rest, format, context, limit, max_bytes)?;
        let truncated = files.len() < rest.len();
        let cursor = files
            .last()
            .filter(|_| truncated)
            .map(|last| last.path.clone());
        let mut out = Diff::new(old.to_string(), new.to_string(), files, truncated);
        out.cursor = cursor;
        Ok(out)
    }

    fn ref_list(&self, req: RefListReq) -> Result<RefListResp, OpError> {
        let prefix = req.prefix.unwrap_or_default();
        let refs = self.ops.repo.list_refs().map_err(error::op)?;

        // 1. The refs whose names start with the prefix, each with its kind.
        let listed = refs
            .into_iter()
            .filter(|(name, _)| name.starts_with(&prefix))
            .map(|(name, state)| {
                let kind = swarmops_store::ref_kind(&name).and_then(kind_out);
                let mut out = RefOut::new(name, state.target.to_string(), state.version);
                out.kind = kind;
                out
            })
            .collect();
        Ok(RefListResp::new(listed))
    }

    fn ref_set(&self, who: &Identity, req: RefSetReq) -> Result<RefSetResp, OpError> {
        let RefSetReq {
            name,
            new,
            expected_old,
            create,
            ..
        } = req;
        check_ref_use(&name, RefUse::Move)?;
        if create == expected_old.is_some() {
            return Err(error::error(
                ErrorCode::InvalidArg,
                format!("ref set {name}: give exactly one of expected_old and create"),
            )
            .with_details(error::details([("name", name.into())]))
            .with_hint(
                "move a ref with --expect <its target now>, or make a new one with --create",
            ));
        }
        // Both revisions resolve in one snapshot, which is not read after the move.
        let read = self.snapshot()?;
        let new = self.resolve(&read, &new)?;
        let expected = match &expected_old {
            Some(rev) => Some(self.resolve(&read, rev)?),
            None => None,
        };
        // main moves only forward: the pair the request names, judged on its snapshot. Ancestry
        // never changes, so a move of main since is the compare-and-swap's to catch.
        read.check_forward(&name, expected, Some(new))
            .map_err(error::op)?;

        // Every move is compared under the lock, after the write's tail check, never on a reader's
        // snapshot, which can show another writer's frame before its sync. A ref that holds `new`
        // already is a replay, whatever `expected` says: a mismatch whose actual target is `new`,
        // or, for a move to the target it expects, the match itself, which the repository does
        // not record.
        let outcome = self
            .ops
            .repo
            .cas_ref(&name, expected, Some(new), who.agent())
            .map_err(error::op)?;
        self.resolved.record_recovered(outcome.recovered);
        let replay = match outcome.result {
            CasResult::Applied { old } => old == Some(new),
            CasResult::Mismatch { actual } if actual == Some(new) => true,
            CasResult::Mismatch { actual } => {
                let message = match (expected, actual) {
                    (Some(_), Some(actual)) => {
                        retry_on(&format!("ref {name} moved to {actual}"), actual)
                    }
                    (None, Some(actual)) => retry_on(
                        &format!("ref {name} exists already and points at {actual}"),
                        actual,
                    ),
                    (Some(expected), None) => {
                        format!("ref {name} does not exist, so it does not point at {expected}")
                    }
                    (None, None) => format!("ref {name} does not exist"),
                };
                return Err(cas_mismatch(&name, actual, message));
            }
            _ => return Err(error::unknown_variant("CasResult")),
        };
        let mut out = RefSetResp::new(name, replay);
        out.old = expected.map(|id| id.to_string());
        out.new = Some(new.to_string());
        out.warnings = outcome.warnings;
        Ok(out)
    }

    fn ref_delete(&self, who: &Identity, req: RefDeleteReq) -> Result<RefSetResp, OpError> {
        let RefDeleteReq {
            name, expected_old, ..
        } = req;
        check_ref_use(&name, RefUse::Move)?;
        let expected = self.resolve(&self.snapshot()?, &expected_old)?;
        let outcome = self
            .ops
            .repo
            .cas_ref(&name, Some(expected), None, who.agent())
            .map_err(error::op)?;
        self.resolved.record_recovered(outcome.recovered);
        let replay = match outcome.result {
            CasResult::Applied { .. } => false,
            // Gone already, as the delete asks: a replay of it, or of another's.
            CasResult::Mismatch { actual: None } => true,
            CasResult::Mismatch {
                actual: Some(actual),
            } => {
                let message =
                    format!("ref {name} now points at {actual}; re-check before deleting");
                return Err(cas_mismatch(&name, Some(actual), message));
            }
            _ => return Err(error::unknown_variant("CasResult")),
        };
        let mut out = RefSetResp::new(name, replay);
        out.old = Some(expected.to_string());
        out.warnings = outcome.warnings;
        Ok(out)
    }

    fn op_log(&self, req: OpLogReq) -> Result<OpLogPage, OpError> {
        let OpLogReq {
            before,
            limit,
            max_changesets,
            ..
        } = req;
        at_least_one("limit", limit)?;
        let (operations, older) = self.ops.repo.op_log(before, limit).map_err(error::op)?;
        // Taken after the listing, the snapshot holds every changeset the operations recorded.
        let read = self.snapshot()?;
        let ops = operations
            .into_iter()
            .map(|operation| operation_out(&read, operation, max_changesets))
            .collect::<Result<_, _>>()?;
        Ok(OpLogPage::new(ops, older))
    }

    fn undo(&self, who: &Identity, req: UndoReq) -> Result<UndoResp, OpError> {
        // The id, parsed before the store's lock is taken: any other form is E_INVALID_ARG.
        let id: OpId = req.id.parse().map_err(|e: OpIdError| {
            error::error(ErrorCode::InvalidArg, e.to_string())
                .with_details(error::details([("id", req.id.clone().into())]))
                .with_hint("swarmops op log")
        })?;
        let outcome = self.ops.repo.undo(id, who.agent()).map_err(|e| {
            // An undo refused under the lock (E_CAS_MISMATCH, E_OP_NOT_FOUND,
            // E_NOTHING_TO_UNDO, E_REF_PROTECTED) reports what the write repaired first, as a ref
            // move's does.
            if let RepoError::UndoBlocked { recovered, .. }
            | RepoError::OpNotFound { recovered, .. }
            | RepoError::NothingToUndo { recovered, .. }
            | RepoError::HistoryProtected { recovered, .. } = &e
            {
                self.resolved.record_recovered(**recovered);
            }

            // An `id` in the details is the id as requested (`004:…` stays), not as parsed.
            let mut err = error::op(e);
            if err.details.contains_key("id") {
                err.details.insert("id".to_owned(), req.id.clone().into());
            }
            err
        })?;
        self.resolved.record_recovered(outcome.recovered);
        let ref_moves = outcome
            .moves
            .into_iter()
            .map(|(name, old, new, at)| {
                let mut moved = RefMoveOut::new(name, who.agent(), at);
                moved.old = old.map(|id| id.to_string());
                moved.new = new.map(|id| id.to_string());
                moved
            })
            .collect();
        let mut out = UndoResp::new(outcome.undone, ref_moves, outcome.already_applied);
        out.op = outcome.frame_seq;
        out.id = outcome
            .frame_seq
            .zip(outcome.frame_crc)
            .map(|(seq, crc)| OpId::new(seq, crc).to_string());
        out.warnings = outcome.warnings;
        Ok(out)
    }

    fn verify(&self, req: VerifyReq) -> Result<VerifyReport, OpError> {
        let depth = if req.deep {
            Depth::Deep
        } else {
            Depth::Shallow
        };
        // 1. The cap: the default, or what the request asks for within 1..=VERIFY_MAX_FINDINGS.
        let max = req.max_findings.unwrap_or(VERIFY_MAX_FINDINGS);
        if !(1..=VERIFY_MAX_FINDINGS).contains(&max) {
            return Err(error::error(
                ErrorCode::InvalidArg,
                format!("max_findings is {max}; it takes 1 to {VERIFY_MAX_FINDINGS}"),
            )
            .with_details(error::details([("max_findings", max.into())]))
            .with_hint(format!(
                "pass a max_findings from 1 to {VERIFY_MAX_FINDINGS}"
            )));
        }
        let max = usize::try_from(max).unwrap_or(usize::MAX);

        // 2. The store-core problems, already cut to `max`; `found` counts them all.
        let report = (self.ops.repo)
            .verify_capped(depth, max)
            .map_err(error::op)?;
        let mut problems: Vec<String> = (report.problems.iter())
            .map(|problem| error::one_line(problem.to_string()))
            .collect();
        let mut findings: Vec<VerifyFinding> = (report.problems.iter())
            .map(finding)
            .collect::<Result<_, _>>()?;
        let mut found = report.found;
        let mut truncated = report.truncated;

        // 3. A deep verify also re-derives the git export's oid memos against the objects, which
        // the store-core verify cannot (rendering git objects lives in swarmops-git): a crafted
        // or torn git_tree_oid, git_blob_oid or git_oid is reported as the other derived values
        // are. And the external id entries of native changesets against their export oids. Each
        // counts in `found`; it is listed only while the report has room.
        let mut extras: Vec<(String, VerifyFinding)> = Vec::new();
        if req.deep {
            let key = self.ops.memo_key.as_ref();
            let mismatches = swarmops_git::verify_export_oids_with(&self.ops.repo, key);
            for mismatch in mismatches.map_err(error::op)? {
                extras.push(git_memo_finding(&mismatch));
            }
            for entry in swarmops_git::verify_external_ids(&self.ops.repo).map_err(error::op)? {
                extras.push(external_id_finding(&entry));
            }
        }
        for (line, extra) in extras {
            found = found.saturating_add(1);
            if problems.len() >= max {
                truncated = true;
                continue;
            }
            problems.push(line);
            findings.push(extra);
        }
        let mut out =
            VerifyReport::new(report.objects_checked, report.changesets_checked, problems);
        out.found = found;
        out.truncated = truncated;
        out.findings = findings;
        Ok(out)
    }

    fn derive(&self, req: DeriveReq) -> Result<DeriveResp, OpError> {
        let DeriveReq { after, limit, .. } = req;
        let run = self
            .ops
            .repo
            .derive_eager(after, limit)
            .map_err(error::op)?;
        let mut out = DeriveResp::new(run.derived, run.cached);
        out.next = run.last_frame;
        Ok(out)
    }

    fn gc(&self, req: GcReq) -> Result<GcResp, OpError> {
        crate::gc::run(self, req)
    }

    fn repair(&self, req: RepairReq) -> Result<RepairResp, OpError> {
        crate::repair::run(self, req)
    }

    fn upgrade(&self, req: UpgradeReq) -> Result<UpgradeResp, OpError> {
        crate::upgrade::run(self, req)
    }
}

/// Each operation as a request of its own, whose resolved revisions go unrecorded: to report
/// them, call it through [`LocalOps::recording`].
impl StoreOps for LocalOps {
    fn repo_info(&self, req: InfoReq) -> Result<RepoInfo, OpError> {
        self.recording(&Resolved::default()).repo_info(req)
    }

    fn commit(&self, who: &Identity, req: CommitReq) -> Result<CommitResp, OpError> {
        self.recording(&Resolved::default()).commit(who, req)
    }

    fn land(&self, who: &Identity, req: LandReq) -> Result<LandResp, OpError> {
        self.recording(&Resolved::default()).land(who, req)
    }

    fn blob_put(&self, who: &Identity, req: BlobPutReq) -> Result<BlobPutResp, OpError> {
        self.recording(&Resolved::default()).blob_put(who, req)
    }

    fn rebase(&self, who: &Identity, req: RebaseReq) -> Result<RebaseResp, OpError> {
        self.recording(&Resolved::default()).rebase(who, req)
    }

    fn blob_get(&self, req: BlobGetReq) -> Result<BlobGetResp, OpError> {
        self.recording(&Resolved::default()).blob_get(req)
    }

    fn blob_has(&self, req: BlobHasReq) -> Result<BlobHasResp, OpError> {
        self.recording(&Resolved::default()).blob_has(req)
    }

    fn resolve_rev(&self, req: ResolveRevReq) -> Result<ResolvedRev, OpError> {
        self.recording(&Resolved::default()).resolve_rev(req)
    }

    fn read_file(&self, req: ReadFileReq) -> Result<FileContent, OpError> {
        self.recording(&Resolved::default()).read_file(req)
    }

    fn read_tree(&self, req: ReadTreeReq) -> Result<TreeListing, OpError> {
        self.recording(&Resolved::default()).read_tree(req)
    }

    fn show(&self, req: ShowReq) -> Result<ChangesetDetail, OpError> {
        self.recording(&Resolved::default()).show(req)
    }

    fn log(&self, req: LogReq) -> Result<LogPage, OpError> {
        self.recording(&Resolved::default()).log(req)
    }

    fn path_history(&self, req: PathHistoryReq) -> Result<PathHistoryPage, OpError> {
        self.recording(&Resolved::default()).path_history(req)
    }

    fn diff(&self, req: DiffReq) -> Result<Diff, OpError> {
        self.recording(&Resolved::default()).diff(req)
    }

    fn ref_list(&self, req: RefListReq) -> Result<RefListResp, OpError> {
        self.recording(&Resolved::default()).ref_list(req)
    }

    fn ref_set(&self, who: &Identity, req: RefSetReq) -> Result<RefSetResp, OpError> {
        self.recording(&Resolved::default()).ref_set(who, req)
    }

    fn ref_delete(&self, who: &Identity, req: RefDeleteReq) -> Result<RefSetResp, OpError> {
        self.recording(&Resolved::default()).ref_delete(who, req)
    }

    fn op_log(&self, req: OpLogReq) -> Result<OpLogPage, OpError> {
        self.recording(&Resolved::default()).op_log(req)
    }

    fn undo(&self, who: &Identity, req: UndoReq) -> Result<UndoResp, OpError> {
        self.recording(&Resolved::default()).undo(who, req)
    }

    fn verify(&self, req: VerifyReq) -> Result<VerifyReport, OpError> {
        self.recording(&Resolved::default()).verify(req)
    }

    fn derive(&self, req: DeriveReq) -> Result<DeriveResp, OpError> {
        self.recording(&Resolved::default()).derive(req)
    }

    fn gc(&self, req: GcReq) -> Result<GcResp, OpError> {
        self.recording(&Resolved::default()).gc(req)
    }

    fn repair(&self, req: RepairReq) -> Result<RepairResp, OpError> {
        self.recording(&Resolved::default()).repair(req)
    }

    fn upgrade(&self, req: UpgradeReq) -> Result<UpgradeResp, OpError> {
        self.recording(&Resolved::default()).upgrade(req)
    }
}

/// What an operation does with the ref it names, which [`check_ref_use`] checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefUse {
    /// `ref set` and `ref delete` move it.
    Move,
    /// `land` and `commit --advance` publish a changeset on it.
    Publish,
    /// `rebase --onto` builds on its head.
    Onto,
}

/// Refuses a ref the operation may not use as `purpose` says, before anything is resolved or
/// read: `E_REF_NAME_INVALID` with `details.reason` `reserved` for a remote-tracking ref, which
/// only a fetch moves, and `tag` for a tag named to publish on, which never moves. Any other
/// name, valid or not, is the store's to check.
pub(crate) fn check_ref_use(name: &str, purpose: RefUse) -> Result<(), OpError> {
    let (reason, message) = match (swarmops_store::ref_kind(name), purpose) {
        (Some(RefKind::RemoteTracking), _) => (
            "reserved",
            format!("{name} is a remote-tracking ref, which only a fetch moves"),
        ),
        (Some(RefKind::Tag), RefUse::Publish) => (
            "tag",
            format!("{name} is a tag, which never moves: land or advance a branch instead"),
        ),
        _ => return Ok(()),
    };
    Err(error::error(ErrorCode::RefNameInvalid, message)
        .with_details(error::details([
            ("name", name.into()),
            ("reason", reason.into()),
        ]))
        .with_hint("swarmops ref list"))
}

/// The format a create of `InitReq.format` makes: this build's default for `None`;
/// `E_INVALID_ARG` for a format this build does not create.
fn init_format(format: Option<u32>) -> Result<StoreFormat, OpError> {
    let Some(version) = format else {
        return Ok(swarmops_store::DEFAULT_INIT_FORMAT);
    };
    let creatable = swarmops_store::INIT_FORMATS;
    if let Some(format) = StoreFormat::from_version(version)
        && creatable.contains(&format)
    {
        return Ok(format);
    }
    let formats: Vec<Value> = creatable.iter().map(|f| f.version().into()).collect();
    Err(error::error(
        ErrorCode::InvalidArg,
        format!("this build does not create stores of format {version}"),
    )
    .with_details(error::details([
        ("format", version.into()),
        ("formats", formats.into()),
    ])))
}

/// The API's name for ref kind `kind`; `None` for a kind this build does not name.
fn kind_out(kind: RefKind) -> Option<swarmops_api::RefKind> {
    match kind {
        RefKind::Main => Some(swarmops_api::RefKind::Main),
        RefKind::Release => Some(swarmops_api::RefKind::Release),
        RefKind::Branch => Some(swarmops_api::RefKind::Branch),
        RefKind::Tag => Some(swarmops_api::RefKind::Tag),
        RefKind::RemoteTracking => Some(swarmops_api::RefKind::RemoteTracking),
        _ => None,
    }
}

fn rev_and_path(rev: &Rev, path: &str) -> Map<String, Value> {
    error::details([("rev", rev.as_str().into()), ("path", path.into())])
}

/// Nothing at `path` in the changeset `rev` named; the hint lists the directory it would be in.
fn path_not_found(rev: &Rev, path: &str) -> OpError {
    error::error(
        ErrorCode::PathNotFound,
        format!("nothing is at {path:?} in {rev}"),
    )
    .with_details(rev_and_path(rev, path))
    .with_hint(listing_hint(rev, path))
}

/// No changeset on `rev`'s first-parent history touched `path`: nothing was ever there. The hint
/// lists the directory it would be in, as [`path_not_found`]'s does.
fn no_history(rev: &Rev, path: &str) -> OpError {
    error::error(
        ErrorCode::PathNotFound,
        format!("no changeset in the first-parent history of {rev} touched {path:?}"),
    )
    .with_details(rev_and_path(rev, path))
    .with_hint(listing_hint(rev, path))
}

/// `swarmops ls` of the directory that `path` would be in at `rev`: the hint of a path that holds
/// nothing there.
fn listing_hint(rev: &Rev, path: &str) -> String {
    let listing = match path.rsplit_once('/') {
        Some((dir, _)) => error::shell_word(&format!("{rev}:{dir}")),
        None => error::shell_word(rev.as_str()),
    };
    error::hint(format!("swarmops ls {listing}"))
}

/// `path` as a repository path, one trailing `/` taken (`src/` is `src`); `E_INVALID_ARG` naming
/// the path as given for one that is not a path even without it (the root's empty path among
/// them).
fn prefix_path(path: &str) -> Result<RepoPath, OpError> {
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    RepoPath::new(trimmed).or_else(|_| content::repo_path(path))
}

/// The filter of a `log` request's `paths`: `None` for none, which keeps every changeset; else
/// the paths as prefixes ([`prefix_path`]).
fn log_filter(paths: &[String]) -> Result<Option<PathFilter>, OpError> {
    if paths.is_empty() {
        return Ok(None);
    }
    let prefixes = paths.iter().map(|path| prefix_path(path));
    Ok(Some(PathFilter::under(prefixes.collect::<Result<_, _>>()?)))
}

/// `event`, one changeset of a path's history in `read`, as `path_history` gives it: what the
/// path holds after it, and the changeset's agent, time and title as `log` gives them.
fn history_entry(read: &Snapshot<'_>, event: &PathEvent) -> Result<PathHistoryEntry, OpError> {
    let ChangesetSummary {
        id,
        author_agent,
        created_at,
        title,
        ..
    } = summary(&stored(read, event.changeset)?);
    let (op, kind, id_after) = match event.entry {
        Some(target) => (
            ChangeOp::Put,
            Some(entry_kind_out(target.kind())?),
            Some(target.id().to_hex()),
        ),
        None => (ChangeOp::Delete, None, None),
    };
    let mut out = PathHistoryEntry::new(id, op, author_agent, created_at, title);
    out.kind = kind;
    out.id = id_after;
    Ok(out)
}

/// The message of a `ref_set` that lost to another move, which left the ref at `actual`
/// (`what` says how): commit the change again on top of `actual`, its edits re-applied to the
/// content there (whole files sent again would revert the other's edits; a land merges them
/// instead), then expect `actual`.
fn retry_on(what: &str, actual: ChangesetId) -> String {
    format!(
        "{what}: commit your change on top of it (--parent {actual}), then retry with --expect \
         {actual}; re-apply your edits to the content at {actual} rather than re-sending your \
         old files"
    )
}

/// A ref move refused because ref `name` names `actual` (`None`: it does not exist), not what
/// the move expected; `message` says so and how to retry. The hint only reads (what the ref
/// names now, or the refs when it is gone), so that an agent that runs it cannot undo the move
/// that won.
pub(crate) fn cas_mismatch(name: &str, actual: Option<ChangesetId>, message: String) -> OpError {
    let next = match actual {
        Some(actual) => format!("swarmops show {actual}"),
        None => "swarmops ref list".to_owned(),
    };
    let actual = actual.map_or(Value::Null, |id| id.to_string().into());
    error::error(ErrorCode::CasMismatch, message)
        .with_details(error::details([("name", name.into()), ("actual", actual)]))
        .with_hint(next)
}

/// `E_INVALID_ARG` unless `value`, the request's `field`, is at least 1.
fn at_least_one(field: &str, value: usize) -> Result<(), OpError> {
    if value == 0 {
        return Err(
            error::error(ErrorCode::InvalidArg, format!("{field} must be at least 1"))
                .with_details(error::details([(field, value.into())])),
        );
    }
    Ok(())
}

/// `E_INVALID_ARG` unless `max_bytes`, of `read_file` or `diff`, is at least 1.
fn at_least_one_byte(max_bytes: u64) -> Result<(), OpError> {
    if max_bytes == 0 {
        return Err(
            error::error(ErrorCode::InvalidArg, "max_bytes must be at least 1")
                .with_details(error::details([("max_bytes", max_bytes.into())])),
        );
    }
    Ok(())
}

/// Test hook, in a build with the `test-hooks` feature: with `SWARMOPS_TEST_NOW=<unix seconds>`
/// set to an integer, [`LocalOps`] signs the changesets it commits at that time instead of the
/// clock's, but for a keyed commit's, which take their key's time either way. The same commit
/// made twice is then the same changeset, whenever it runs: a crash test's runs write the same
/// objects, and so make the same filesystem calls, which the store's own test hooks count and
/// crash at (`swarmops_store::CRASH_AT_ENV`, `swarmops_store::FS_CALLS_ENV`). Unset, not an
/// integer, or in a build without the feature (a release build among them): the clock's time.
pub const NOW_ENV: &str = "SWARMOPS_TEST_NOW";

/// Now, in Unix seconds: [`NOW_ENV`]'s seconds if the test hook gives them ([`test_now`]),
/// else the clock's.
fn signing_time() -> i64 {
    if let Some(secs) = test_now() {
        return secs;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// [`NOW_ENV`]'s seconds, if it is set to an integer, in a build with the `test-hooks` feature.
#[cfg(feature = "test-hooks")]
fn test_now() -> Option<i64> {
    std::env::var(NOW_ENV)
        .ok()
        .and_then(|secs| secs.parse().ok())
}

/// In a build without the `test-hooks` feature, [`NOW_ENV`] is ignored.
#[cfg(not(feature = "test-hooks"))]
fn test_now() -> Option<i64> {
    None
}

/// The metadata key under which a keyed commit records its key.
const COMMIT_KEY: &str = "commit_key";

/// A commit's idempotency key (`CommitReq::idempotency_key`, `commit --key`): a uuid v7 (RFC
/// 9562), whose first 48 bits are its time in Unix milliseconds.
struct CommitKey {
    /// The key as a changeset's `commit_key` metadata records it: 36 characters, lowercase.
    text: String,
    /// Its time in whole Unix seconds, which a keyed commit signs at, so that the same request
    /// made again is the same changeset.
    secs: i64,
}

impl CommitKey {
    /// `key`, if it is a uuid v7: 32 hex digits of either case in groups of 8, 4, 4, 4 and 12,
    /// a hyphen between each two, the version digit (the 15th character) 7, and the variant
    /// digit (the 20th) 8, 9, a or b, for the variant `10`. Otherwise `E_INVALID_ARG`, naming
    /// it in `details.idempotency_key`.
    fn parse(key: &str) -> Result<CommitKey, OpError> {
        let refused = |why: &str| {
            error::error(
                ErrorCode::InvalidArg,
                format!("idempotency_key {key:?} is not a uuid v7: {why}"),
            )
            .with_details(error::details([("idempotency_key", key.into())]))
            .with_hint("make a new uuid v7 for each commit, and give it again only to retry it")
        };
        let bytes = key.as_bytes();
        let shaped = bytes.len() == 36
            && bytes.iter().enumerate().all(|(at, byte)| match at {
                8 | 13 | 18 | 23 => *byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            });
        if !shaped {
            return Err(refused(
                "a uuid is 32 hex digits in groups of 8, 4, 4, 4 and 12, a hyphen between each two",
            ));
        }
        let (version, variant) = (char::from(bytes[14]), char::from(bytes[19]));
        if version != '7' {
            return Err(refused(&format!("its version digit is {version}, not 7")));
        }
        if !matches!(variant, '8' | '9' | 'a' | 'b' | 'A' | 'B') {
            return Err(refused(&format!(
                "its variant digit is {variant}, not 8, 9, a or b"
            )));
        }
        let text = key.to_ascii_lowercase();
        // The first 12 hex digits: the time.
        let millis = (text.chars())
            .filter(|c| *c != '-')
            .take(12)
            .filter_map(|c| c.to_digit(16))
            .fold(0_u64, |millis, digit| (millis << 4) | u64::from(digit));
        let secs = i64::try_from(millis / 1000).unwrap_or(i64::MAX);
        Ok(CommitKey { text, secs })
    }

    /// Whether a changeset whose metadata is `metadata` records this key: the changeset a
    /// request with it made.
    fn made(&self, metadata: &[(String, Vec<u8>)]) -> bool {
        (metadata.iter()).any(|(key, value)| key == COMMIT_KEY && value == self.text.as_bytes())
    }
}

/// Whom a changeset's `metadata` attributes it to.
fn attribution(metadata: &[(String, Vec<u8>)]) -> Attribution {
    let mut out = Attribution::new(
        metadata_text(metadata, "agent").unwrap_or_else(|| UNKNOWN_AGENT.to_owned()),
    );
    out.run_id = metadata_text(metadata, "run_id");
    out
}

/// Who landed a changeset whose metadata is `metadata`, if a land made it: its `lander` and, when
/// present, its `lander_run_id`.
fn lander(metadata: &[(String, Vec<u8>)]) -> Option<Attribution> {
    let mut out = Attribution::new(metadata_text(metadata, LANDER_KEY)?);
    out.run_id = metadata_text(metadata, LANDER_RUN_ID_KEY);
    Some(out)
}

/// The value of `key` in a changeset's `metadata`, as text (invalid UTF-8 replaced), if it has
/// the key.
fn metadata_text(metadata: &[(String, Vec<u8>)], key: &str) -> Option<String> {
    (metadata.iter())
        .find(|(k, _)| k == key)
        .map(|(_, value)| lossy(value))
}

/// `E_INVALID_ARG`, naming it in `details.idempotency_key`, unless `key`, a land's idempotency
/// key, is 1 to 64 ASCII letters, digits and `-` (operations spec §3 (a)): a uuid v7, or a
/// changeset id's 64 hex digits.
fn land_key(key: &str) -> Result<(), OpError> {
    let allowed = |b: u8| b.is_ascii_alphanumeric() || b == b'-';
    if (1..=MAX_LAND_KEY).contains(&key.len()) && key.bytes().all(allowed) {
        return Ok(());
    }
    Err(error::error(
        ErrorCode::InvalidArg,
        format!("idempotency_key {key:?} is not 1 to {MAX_LAND_KEY} letters, digits and '-'"),
    )
    .with_details(error::details([("idempotency_key", key.into())]))
    .with_hint("make a new uuid v7 for each land, and give it again only to retry that land"))
}

/// A land's or a rebase's error `e` as an [`OpError`]: a conflict's report carries at most
/// `max_conflict_bytes` of hunk text.
fn merge_error(e: RepoError, max_conflict_bytes: u64) -> OpError {
    let message = e.to_string();
    match e {
        RepoError::Conflict(conflicts) => {
            crate::error::conflict(&conflicts, message, max_conflict_bytes)
        }
        e => error::op(e),
    }
}

/// A land's or a rebase's change, resolved ([`Recording::change`]).
struct ResolvedChange {
    /// Its head.
    head: ChangesetId,
    /// Its base, if the request names one.
    base: Option<ChangesetId>,
    /// What a land expects its target to name, if it does.
    expected: Option<ChangesetId>,
    /// Its resolutions, read.
    resolutions: Option<Resolutions>,
}

/// A land's resolutions as the request gives them, their shape checked before anything is
/// resolved or read ([`shaped_resolutions`]).
struct ShapedResolutions {
    /// The revision they were decided against.
    against: Rev,
    /// Each resolution, with its path, in path order.
    files: Vec<(RepoPath, Shaped)>,
}

/// One resolution of a land, its shape checked.
enum Shaped {
    /// What one side holds there.
    Take(swarmops_api::Side),
    /// New content, not yet converted, and the mode it gives, if any.
    Content(ContentInput, Option<ModeInput>),
    /// A blob the store is to hold, by its id, and the mode it gives, if any.
    Blob(swarmops_core::BlobId, Option<ModeInput>),
    /// Nothing there.
    Delete,
}

/// `input`, its shape checked (operations spec §3, resolution path (b)): each path a repository
/// path, each resolution exactly one of `take`, `content` and `delete`, a `mode` only beside
/// `content`, and a blob it names by id an id. `E_INVALID_ARG` otherwise, naming the path in its
/// message and its `details` ([`content::in_resolution`]).
fn shaped_resolutions(input: ResolutionsInput) -> Result<ShapedResolutions, OpError> {
    let ResolutionsInput { against, files, .. } = input;
    let files = (files.into_iter())
        .map(|(path, resolution)| {
            let refuse = |what: String| {
                content::in_resolution(error::error(ErrorCode::InvalidArg, what), path.as_str())
            };
            let at = RepoPath::new(path.as_str())
                .map_err(|e| refuse(format!("not a repository path: {e}")))?;
            let ResolutionInput {
                take,
                content,
                mode,
                delete,
                ..
            } = resolution;
            let given: Vec<&str> = [
                ("take", take.is_some()),
                ("content", content.is_some()),
                ("delete", delete),
            ]
            .into_iter()
            .filter_map(|(field, given)| given.then_some(field))
            .collect();
            let shaped = match (take, content, delete) {
                (Some(side), None, false) if mode.is_none() => Shaped::Take(side),
                (None, Some(ContentInput::Blob(id)), false) => {
                    let id = content::blob_id(&id)
                        .map_err(|e| content::in_resolution(e, path.as_str()))?;
                    Shaped::Blob(id, mode)
                }
                (None, Some(content), false) => Shaped::Content(content, mode),
                (None, None, true) if mode.is_none() => Shaped::Delete,
                _ if given.len() == 1 => {
                    let only = given[0];
                    return Err(refuse(format!(
                        "a mode goes with content only, not with {only}"
                    )));
                }
                _ if given.is_empty() => {
                    return Err(refuse("give one of take, content and delete".to_owned()));
                }
                _ => {
                    let given = given.join(" and ");
                    return Err(refuse(format!(
                        "give exactly one of take, content and delete, not {given}"
                    )));
                }
            };
            Ok((at, shaped))
        })
        .collect::<Result<Vec<_>, OpError>>()?;
    Ok(ShapedResolutions { against, files })
}

impl ShapedResolutions {
    /// The repository's resolutions, against `against`, the changeset `self.against` named: a
    /// side, a delete, a blob by id, or new content, converted by a commit's rules for contents
    /// ([`content::resolution_bytes`]), each of the kind its mode gives, if it gives one.
    fn resolutions(self, against: ChangesetId) -> Result<Resolutions, OpError> {
        let kind =
            |mode: Option<ModeInput>| mode.map(|mode| content::leaf_kind(Some(mode))).transpose();
        let mut files = BTreeMap::new();
        let (mut contents, mut modes) = (Vec::new(), Vec::new());
        for (path, shaped) in self.files {
            let resolution = match shaped {
                Shaped::Take(swarmops_api::Side::Ours) => {
                    Resolution::Take(swarmops_repo::Side::Ours)
                }
                Shaped::Take(swarmops_api::Side::Theirs) => {
                    Resolution::Take(swarmops_repo::Side::Theirs)
                }
                Shaped::Take(_) => return Err(error::unknown_variant("Side")),
                Shaped::Blob(id, mode) => Resolution::PutBlob {
                    kind: kind(mode)?,
                    id,
                },
                Shaped::Delete => Resolution::Delete,
                Shaped::Content(content, mode) => {
                    contents.push((path, content));
                    modes.push(mode);
                    continue;
                }
            };
            files.insert(path, resolution);
        }
        let converted = content::resolution_bytes(contents)?;
        for ((path, content), mode) in converted.into_iter().zip(modes) {
            let kind = kind(mode)?;
            files.insert(path, Resolution::Put { kind, content });
        }
        Ok(Resolutions::new(against, files))
    }
}

/// When a changeset with `committer` was recorded, in Unix milliseconds: 0 before 1970.
fn created_at(committer: &Signature) -> u64 {
    u64::try_from(committer.secs).map_or(0, |secs| secs.saturating_mul(1000))
}

fn summary(stored: &StoredChangeset) -> ChangesetSummary {
    let changeset = &stored.changeset;
    ChangesetSummary::new(
        stored.id.to_string(),
        changeset.parents.iter().map(ToString::to_string).collect(),
        attribution(&changeset.metadata).agent,
        created_at(&changeset.committer),
        title(&changeset.message),
    )
}

/// A message's first line, without its line break (`\n` or `\r\n`), as text.
fn title(message: &[u8]) -> String {
    let first_line = message.split(|&b| b == b'\n').next().unwrap_or_default();
    lossy(first_line.strip_suffix(b"\r").unwrap_or(first_line))
}

/// `operation` as `op_log` gives it: its kind ([`operation_kind`]), its first
/// `max_changesets` changesets, each read in `read` for its agent, time, title and land key, how
/// many it recorded in all, and its ref moves.
fn operation_out(
    read: &Snapshot<'_>,
    operation: Operation,
    max_changesets: usize,
) -> Result<OperationOut, OpError> {
    let Operation {
        seq, crc, records, ..
    } = operation;
    let ids: Vec<ChangesetId> = records
        .iter()
        .filter_map(|record| match record {
            OpRecord::Changeset { id, .. } => Some(Ok(*id)),
            OpRecord::RefMove { .. } | OpRecord::Compacted => None,
            _ => Some(Err(error::unknown_variant("OpRecord"))),
        })
        .collect::<Result<_, _>>()?;
    let listed = ids
        .iter()
        .take(max_changesets)
        .map(|&id| stored(read, id))
        .collect::<Result<Vec<_>, _>>()?;
    let kind = operation_kind(&records, |id| {
        let lands = |stored: &StoredChangeset| metadata_value(stored, LANDER_KEY).is_some();
        match listed.iter().find(|stored| stored.id == id) {
            Some(stored) => Ok(lands(stored)),
            None => Ok(lands(&stored(read, id)?)),
        }
    })?;
    let ref_moves = records
        .into_iter()
        .filter_map(|record| match record {
            OpRecord::RefMove {
                name,
                old,
                new,
                actor,
                ts,
            } => {
                let mut moved = RefMoveOut::new(name, actor, ts);
                moved.old = old.map(|id| id.to_string());
                moved.new = new.map(|id| id.to_string());
                Some(Ok(moved))
            }
            OpRecord::Changeset { .. } | OpRecord::Compacted => None,
            _ => Some(Err(error::unknown_variant("OpRecord"))),
        })
        .collect::<Result<_, _>>()?;
    Ok(OperationOut::new(
        seq,
        OpId::new(seq, crc).to_string(),
        kind,
        listed.iter().map(op_changeset_out).collect(),
        bytes::to_u64(ids.len()),
        ref_moves,
    ))
}

/// What an operation that recorded `records` did: one changeset and a move of a ref to it is a
/// land when `lands` says the changeset's metadata names a lander (every land's does; its
/// `land_key` is optional), else a publish; changesets alone a commit; ref moves alone a ref
/// move; a compaction marker alone a compaction; anything else a mix. `lands` is asked only of
/// that one changeset.
fn operation_kind(
    records: &[OpRecord],
    lands: impl FnOnce(ChangesetId) -> Result<bool, OpError>,
) -> Result<OperationKind, OpError> {
    let mut changesets = Vec::new();
    let mut moved_to = Vec::new();
    let mut compactions = 0_usize;
    for record in records {
        match record {
            OpRecord::Changeset { id, .. } => changesets.push(*id),
            OpRecord::RefMove { new, .. } => moved_to.push(*new),
            OpRecord::Compacted => compactions += 1,
            _ => return Err(error::unknown_variant("OpRecord")),
        }
    }
    Ok(match (&changesets[..], &moved_to[..], compactions) {
        ([id], [Some(to)], 0) if to == id => {
            if lands(*id)? {
                OperationKind::Land
            } else {
                OperationKind::Publish
            }
        }
        ([_, ..], [], 0) => OperationKind::Commit,
        ([], [_, ..], 0) => OperationKind::RefMove,
        ([], [], 1) => OperationKind::Compaction,
        _ => OperationKind::Mixed,
    })
}

/// A changeset an operation recorded, as the operation log lists it.
fn op_changeset_out(stored: &StoredChangeset) -> OpChangesetOut {
    let changeset = &stored.changeset;
    let mut out = OpChangesetOut::new(
        stored.id.to_string(),
        attribution(&changeset.metadata).agent,
        created_at(&changeset.committer),
        title(&changeset.message),
    );
    out.land_key = metadata_value(stored, LAND_KEY_KEY).map(lossy);
    out
}

/// The value of metadata `key` of the changeset `stored`, if it has the key.
fn metadata_value<'s>(stored: &'s StoredChangeset, key: &str) -> Option<&'s [u8]> {
    stored
        .changeset
        .metadata
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, value)| value.as_slice())
}

/// A delta entry as `show` lists it. `E_INTERNAL` for an op this build does not know.
fn change_out(entry: &DeltaEntry) -> Result<ChangeOut, OpError> {
    let path = entry.path.to_string();
    match &entry.op {
        DeltaOp::Put { kind, id } => {
            let mut out = ChangeOut::new(path, ChangeOp::Put);
            out.kind = Some(file_kind(*kind)?);
            out.blob = Some(id.to_string());
            Ok(out)
        }
        DeltaOp::Delete => Ok(ChangeOut::new(path, ChangeOp::Delete)),
        _ => Err(error::unknown_variant("DeltaOp")),
    }
}

/// A git-export oid memo a deep verify found wrong ([`swarmops_git::GitMemoMismatch`]) as a
/// `derived_mismatch` line and finding: `git_oid` names its changeset, `git_tree_oid` and
/// `git_blob_oid` their object. No store path, only ids (storage subspec §5, §7).
fn git_memo_finding(mismatch: &swarmops_git::GitMemoMismatch) -> (String, VerifyFinding) {
    let subject = mismatch.subject.to_hex();
    let line = error::one_line(format!(
        "the derived value {memo} of {subject} is not the one the store's objects give \
         (memoized {stored}, the objects give {real}); remove its file under \
         meta/derived/{memo}/, or all of meta/derived/, to have it derived again",
        memo = mismatch.memo,
        stored = mismatch.stored,
        real = mismatch.real,
    ));
    // `git_oid` ([`swarmops_git::ExportOid`]) and `git_oid_mac`, the same under a memo key, are a
    // changeset's export oid; the other memos are a tree's and a blob's. A memo under a key names
    // the key's id as its key.
    let is_commit = matches!(mismatch.memo.as_str(), "git_oid" | "git_oid_mac");
    let mut found = VerifyFinding::new("derived_mismatch".to_owned());
    found.object = (!is_commit).then(|| subject.clone());
    found.changeset = is_commit.then_some(subject);
    found.key = (!mismatch.key.is_empty()).then(|| external_ids::lower_hex(&mismatch.key));
    found.derivation = Some(mismatch.memo.clone());
    (line, found)
}

/// An external id entry of a native changeset that a deep verify found naming another commit than
/// the one the changeset exports to ([`swarmops_git::ExternalIdMismatch`]) as an
/// `external_id_mismatch` line and finding, as the store-core verify reports an entry whose
/// changeset's `git.oid` is another commit.
fn external_id_finding(entry: &swarmops_git::ExternalIdMismatch) -> (String, VerifyFinding) {
    let key = entry.commit.to_string();
    let line = error::one_line(format!(
        "external id {key} of system git names changeset {}, which holds no git.oid and \
         exports to another commit",
        entry.changeset
    ));
    let mut found = VerifyFinding::new("external_id_mismatch".to_owned());
    found.changeset = Some(entry.changeset.to_string());
    found.system = Some("git".to_owned());
    found.key = Some(key);
    (line, found)
}

/// `problem` as a program reads it (operations spec §1, `verify`): its kind, a string of its
/// own for each variant, and what it names, ids in lowercase hex. `E_INTERNAL` for a kind of
/// problem this build does not know.
fn finding(problem: &VerifyProblem) -> Result<VerifyFinding, OpError> {
    let of = |kind: &str| VerifyFinding::new(kind.to_owned());
    let found = match problem {
        VerifyProblem::CorruptObject(id) => {
            let mut found = of("corrupt_object");
            found.object = Some(id.to_hex());
            found
        }
        VerifyProblem::MissingObject { referenced_by, id } => {
            let mut found = of("missing_object");
            found.object = Some(id.to_hex());
            found.changeset = Some(referenced_by.to_string());
            found
        }
        VerifyProblem::MissingParent { child, parent } => {
            let mut found = of("missing_parent");
            found.changeset = Some(child.to_string());
            found.parent = Some(parent.to_string());
            found
        }
        VerifyProblem::RootMismatch(id) => {
            let mut found = of("root_mismatch");
            found.changeset = Some(id.to_string());
            found
        }
        VerifyProblem::DanglingRef(name) => {
            let mut found = of("dangling_ref");
            found.ref_name = Some(name.clone());
            found
        }
        VerifyProblem::CorruptRef(name) => {
            let mut found = of("corrupt_ref");
            found.ref_name = Some(name.clone());
            found
        }
        VerifyProblem::IndexMismatch { seq, cause } => {
            let mut found = of("index_mismatch");
            found.seq = Some(*seq);
            found.cause = Some(mismatch_cause(*cause)?.to_owned());
            found
        }
        VerifyProblem::JournalCorrupt { offset } => {
            let mut found = of("journal_corrupt");
            found.offset = Some(*offset);
            found
        }
        VerifyProblem::MetaCorrupt(_) => of("meta_corrupt"),
        VerifyProblem::ExternalIdCorrupt { system, key } => {
            let mut found = of("external_id_corrupt");
            found.system = Some(system.clone());
            found.key = Some(key.clone());
            found
        }
        VerifyProblem::ExternalIdDangling {
            system,
            key,
            changeset,
        } => {
            let mut found = of("external_id_dangling");
            found.changeset = Some(changeset.to_string());
            found.system = Some(system.clone());
            found.key = Some(key.clone());
            found
        }
        VerifyProblem::ExternalIdMismatch {
            system,
            key,
            changeset,
        } => {
            let mut found = of("external_id_mismatch");
            found.changeset = Some(changeset.to_string());
            found.system = Some(system.clone());
            found.key = Some(key.clone());
            found
        }
        VerifyProblem::DerivedMismatch {
            name,
            changeset,
            key,
        } => {
            let mut found = of("derived_mismatch");
            found.changeset = Some(changeset.to_string());
            found.key = (!key.is_empty()).then(|| external_ids::lower_hex(key));
            found.derivation = Some(name.clone());
            found
        }
        _ => return Err(error::unknown_variant("VerifyProblem")),
    };
    Ok(found)
}

/// What a finding's `cause` says disagrees, which names what mends it. `E_INTERNAL` for a
/// cause this build does not know.
fn mismatch_cause(cause: MismatchCause) -> Result<&'static str, OpError> {
    match cause {
        MismatchCause::Journal => Ok("journal"),
        MismatchCause::SortedView => Ok("sorted_view"),
        MismatchCause::Object => Ok("object"),
        _ => Err(error::unknown_variant("MismatchCause")),
    }
}

/// Whether a `read_tree` entry carries its `path` beside its `name`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryPath {
    /// A listing one level deep: the name says where the entry is.
    Omitted,
    /// A listing past one level: entries of several directories mix, so each names its path.
    Included,
}

impl EntryPath {
    /// The choice for a listing `depth` levels deep: the path past 1.
    fn when_deep(depth: u32) -> EntryPath {
        if depth > 1 {
            return EntryPath::Included;
        }
        EntryPath::Omitted
    }
}

/// `entry` of a listing as `read_tree` gives it, with its `path` as `path` says.
fn tree_entry_out(entry: &ListedEntry, path: EntryPath) -> Result<TreeEntryOut, OpError> {
    let mut out = TreeEntryOut::new(
        entry.path.file_name().to_owned(),
        entry_kind_out(entry.target.kind())?,
        entry.target.id().to_hex(),
    );
    out.path = (path == EntryPath::Included).then(|| entry.path.to_string());
    out.size = entry.size;
    Ok(out)
}

/// What a tree entry of `kind` is, as the API names it. `E_INTERNAL` for a kind this build does
/// not know.
fn entry_kind_out(kind: EntryKind) -> Result<EntryKindOut, OpError> {
    match kind {
        EntryKind::File => Ok(EntryKindOut::File),
        EntryKind::Exec => Ok(EntryKindOut::Exec),
        EntryKind::Symlink => Ok(EntryKindOut::Symlink),
        EntryKind::Dir => Ok(EntryKindOut::Dir),
        _ => Err(error::unknown_variant("EntryKind")),
    }
}

/// The kind of a file: a leaf, which a file entry and a delta put always are. `E_INTERNAL` for
/// a kind this build does not know.
fn file_kind(kind: LeafKind) -> Result<FileKind, OpError> {
    match kind {
        LeafKind::File => Ok(FileKind::File),
        LeafKind::Exec => Ok(FileKind::Exec),
        LeafKind::Symlink => Ok(FileKind::Symlink),
        _ => Err(error::unknown_variant("LeafKind")),
    }
}

fn signature_out(signature: &Signature) -> SignatureOut {
    SignatureOut::new(
        lossy(&signature.name),
        lossy(&signature.email),
        signature.secs,
        signature.offset_secs,
    )
}

/// `bytes` as UTF-8, with invalid bytes replaced.
fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use swarmops_api::{ChangeInput, ContentInput, RecoveredOut, RefSetReq};
    use swarmops_store::FsOps;
    use swarmops_testkit::faults::FsyncModelFs;
    use swarmops_testkit::fs::FailingFs;
    use swarmops_testkit::store::file_len;

    use super::*;

    /// The real filesystem, except that calls of `method` on a path ending in `suffix` fail
    /// with an injected error.
    fn failing(method: &'static str, suffix: &'static str) -> FailingFs {
        FailingFs::new(move |called, path| called == method && path.ends_with(suffix))
    }

    /// `LocalOps` over the store at `root`, its every filesystem call going through `fs`.
    fn ops_on(root: &Utf8Path, fs: impl FsOps + 'static) -> LocalOps {
        let repo = LocalRepo::open_with_fs(root, Arc::new(fs)).expect("open the store");
        LocalOps::with_repo(repo, root)
    }

    /// An operation's kind comes from its records alone, but for a land, told from a publish by
    /// its changeset's metadata, which is read for that changeset only.
    #[test]
    fn an_operation_s_kind_comes_from_its_records() {
        let (a, b) = (
            ChangesetId::new(ObjectId::new([1; 32])),
            ChangesetId::new(ObjectId::new([2; 32])),
        );
        let inserted = |id| OpRecord::Changeset {
            id,
            parents: Vec::new(),
        };
        let moved = |new| OpRecord::RefMove {
            name: "main".to_owned(),
            old: None,
            new,
            actor: "test/agent".to_owned(),
            ts: 0,
        };
        let kind = |records: &[OpRecord], lander: bool| {
            operation_kind(records, |id| {
                assert_eq!(id, a, "only the published changeset is read");
                Ok(lander)
            })
            .unwrap()
        };
        let publish = [inserted(a), moved(Some(a))];
        assert_eq!(kind(&publish, false), OperationKind::Publish);
        assert_eq!(kind(&publish, true), OperationKind::Land);
        let moved_first = [moved(Some(a)), inserted(a)];
        assert_eq!(kind(&moved_first, true), OperationKind::Land);
        for (records, want) in [
            (&[inserted(a)][..], OperationKind::Commit),
            (&[inserted(a), inserted(b)], OperationKind::Commit),
            (&[moved(Some(a))], OperationKind::RefMove),
            (&[moved(Some(a)), moved(None)], OperationKind::RefMove),
            (&[OpRecord::Compacted], OperationKind::Compaction),
            (&[inserted(a), moved(Some(b))], OperationKind::Mixed),
            (&[inserted(a), moved(None)], OperationKind::Mixed),
            (
                &[inserted(a), inserted(b), moved(Some(a))],
                OperationKind::Mixed,
            ),
            (
                &[inserted(a), moved(Some(a)), moved(Some(a))],
                OperationKind::Mixed,
            ),
            (&[inserted(a), OpRecord::Compacted], OperationKind::Mixed),
            (
                &[OpRecord::Compacted, OpRecord::Compacted],
                OperationKind::Mixed,
            ),
        ] {
            assert_eq!(kind(records, true), want, "{records:?}");
        }
    }

    /// The one warning `warnings` holds, which must name the injected failure.
    #[track_caller]
    fn one_injected(warnings: &[String]) {
        let [warning] = warnings else {
            panic!("expected one warning, got {warnings:?}");
        };
        assert!(warning.contains("injected failure"), "{warning}");
        assert!(warning.contains("committed"), "{warning}");
    }

    /// The external id index's problems, each naming the entry `key` of system `git`, and the
    /// changeset `child` where the problem names one.
    fn external_id_cases(key: &str, child: ChangesetId) -> [VerifyProblem; 3] {
        let (system, key) = ("git".to_owned(), key.to_owned());
        [
            VerifyProblem::ExternalIdCorrupt {
                system: system.clone(),
                key: key.clone(),
            },
            VerifyProblem::ExternalIdDangling {
                system: system.clone(),
                key: key.clone(),
                changeset: child,
            },
            VerifyProblem::ExternalIdMismatch {
                system,
                key,
                changeset: child,
            },
        ]
    }

    /// Each problem `verify` can find, `IndexMismatch` once for each cause, is a finding of its
    /// own kind with exactly the keys that kind names, its ids in lowercase hex, so that a
    /// program reads what is damaged without parsing the line. No two problems share a kind but
    /// `IndexMismatch`'s causes, which `cause` tells apart, and `DerivedMismatch`'s derivations,
    /// which `derivation` does (with a key in lowercase hex where the value has one). The finding
    /// each problem should be comes from an exhaustive match, as `finding` does its own: a new
    /// problem, or a new cause, does not compile here until it is given its kind and keys, and
    /// with them a case.
    #[test]
    fn every_verify_problem_is_a_finding_with_its_keys() {
        let (object, child, parent) = ("ab".repeat(32), "cd".repeat(32), "ef".repeat(32));
        let key = "12".repeat(20);
        let id = |hex: &str| ObjectId::from_hex(&hex.to_uppercase()).unwrap();
        let changeset = |hex: &str| ChangesetId::new(id(hex));
        let mismatch = |cause| VerifyProblem::IndexMismatch { seq: 7, cause };
        let expected = |problem: &VerifyProblem| match problem {
            VerifyProblem::CorruptObject(_) => json!({"kind": "corrupt_object", "object": object}),
            VerifyProblem::MissingObject { .. } => {
                json!({"kind": "missing_object", "object": object, "changeset": child})
            }
            VerifyProblem::MissingParent { .. } => {
                json!({"kind": "missing_parent", "changeset": child, "parent": parent})
            }
            VerifyProblem::RootMismatch(_) => json!({"kind": "root_mismatch", "changeset": child}),
            VerifyProblem::DanglingRef(_) => json!({"kind": "dangling_ref", "ref": "main"}),
            VerifyProblem::CorruptRef(_) => json!({"kind": "corrupt_ref", "ref": "release/1.0"}),
            VerifyProblem::IndexMismatch { cause, .. } => {
                let cause = match cause {
                    MismatchCause::Journal => "journal",
                    MismatchCause::SortedView => "sorted_view",
                    MismatchCause::Object => "object",
                    _ => panic!("a variant this test does not know"),
                };
                json!({"kind": "index_mismatch", "seq": 7, "cause": cause})
            }
            VerifyProblem::JournalCorrupt { .. } => {
                json!({"kind": "journal_corrupt", "offset": 4096})
            }
            VerifyProblem::MetaCorrupt(_) => json!({"kind": "meta_corrupt"}),
            VerifyProblem::ExternalIdCorrupt { .. } => {
                json!({"kind": "external_id_corrupt", "system": "git", "key": key})
            }
            VerifyProblem::ExternalIdDangling { .. } => {
                json!({"kind": "external_id_dangling", "changeset": child, "system": "git", "key": key})
            }
            VerifyProblem::ExternalIdMismatch { .. } => {
                json!({"kind": "external_id_mismatch", "changeset": child, "system": "git", "key": key})
            }
            VerifyProblem::DerivedMismatch { key: path, .. } if path.is_empty() => json!({
                "kind": "derived_mismatch", "changeset": child,
                "derivation": "first_parent_skip_list"
            }),
            VerifyProblem::DerivedMismatch { .. } => json!({
                "kind": "derived_mismatch", "changeset": child, "key": "0161",
                "derivation": "last_modified"
            }),
            _ => panic!("a variant this test does not know"),
        };
        let cases = [
            VerifyProblem::CorruptObject(id(&object)),
            VerifyProblem::MissingObject {
                referenced_by: changeset(&child),
                id: id(&object),
            },
            VerifyProblem::MissingParent {
                child: changeset(&child),
                parent: changeset(&parent),
            },
            VerifyProblem::RootMismatch(changeset(&child)),
            VerifyProblem::DanglingRef("main".to_owned()),
            VerifyProblem::CorruptRef("release/1.0".to_owned()),
            mismatch(MismatchCause::Journal),
            mismatch(MismatchCause::SortedView),
            mismatch(MismatchCause::Object),
            VerifyProblem::JournalCorrupt { offset: 4096 },
            VerifyProblem::MetaCorrupt("journal.off is missing".to_owned()),
        ];
        let external = external_id_cases(&key, changeset(&child));
        // A derived value under a path's key (`a`, as postcard writes it) and one without a key.
        let derived = [
            ("last_modified", vec![1, b'a']),
            ("first_parent_skip_list", Vec::new()),
        ]
        .map(|(name, key)| VerifyProblem::DerivedMismatch {
            name: name.to_owned(),
            changeset: changeset(&child),
            key,
        });
        let mut kinds: BTreeMap<String, Vec<&VerifyProblem>> = BTreeMap::new();
        for problem in cases.iter().chain(&external).chain(&derived) {
            let found = finding(problem).unwrap();
            assert_eq!(
                serde_json::to_value(&found).unwrap(),
                expected(problem),
                "{problem:?}"
            );
            kinds.entry(found.kind).or_default().push(problem);
        }
        assert_eq!(kinds.len(), 13, "{kinds:?}");
        for (kind, problems) in &kinds {
            let shared = problems.len() > 1;
            let told_apart = ["index_mismatch", "derived_mismatch"];
            assert!(
                !shared || told_apart.contains(&kind.as_str()),
                "{kind}: {problems:?}"
            );
        }
    }

    #[test]
    fn failures_after_the_commit_point_come_back_as_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        drop(LocalOps::init_as(&root, "acme", Some(1)).unwrap());
        let who = Identity::parse("agent/worker-17").unwrap();

        // Materializing the commit's frame into the index fails once the frame is durable: the
        // commit succeeded, and says what went wrong after.
        let ops = ops_on(&root, failing("append", "idx/ids.col"));
        let mut put = ChangeInput::new("f".to_owned(), ChangeOp::Put);
        put.content = Some(ContentInput::Inline("x".to_owned()));
        let req = CommitReq::new(vec![put], "m".to_owned());
        let committed = ops.commit(&who, req).unwrap();
        one_injected(&committed.warnings);
        assert_eq!(committed.files_changed, 1);
        drop(ops);

        // Writing the ref file fails once the move's frame is durable: likewise.
        let ops = ops_on(&root, failing("atomic_replace", "refs/main"));
        let mut req = RefSetReq::new("main".to_owned(), committed.changeset.as_str());
        req.create = true;
        let moved = ops.ref_set(&who, req).unwrap();
        one_injected(&moved.warnings);
        assert!(!moved.already_applied);
        drop(ops);

        // Both are committed: a fresh handle sees them.
        let fresh = LocalOps::open(&root).unwrap();
        let info = fresh.repo_info(InfoReq::default()).unwrap();
        assert_eq!(info.main.as_deref(), Some(committed.changeset.as_str()));
        assert_eq!(info.changesets, 1);
    }

    /// A journal append that the filesystem refuses in a way that running the same command again
    /// meets again, for want of permission (`EACCES`) or as a read-only filesystem (`EROFS`), in
    /// each of the store's three ways to `NotCommitted` (storage spec §2 rule 3 (a)-(c)): the
    /// non-retryable `E_IO`, `committed: false`, naming the append, with a hint that says what to
    /// fix; nothing of the request was committed, and once the cause is gone it commits (ruling
    /// M16-R8). Any other failure there is the retryable `E_IO_TRANSIENT`, as the tests beside
    /// this module show.
    mod refused_for_good {
        use std::io;

        use swarmops_store::RealFs;
        use swarmops_testkit::fs::{Call, Watch, Watched};

        use super::*;

        /// Refuses each call whose method and path's end it lists before the call runs, with the
        /// operating system's error it gives: `EACCES` (13) as a file without write permission
        /// does, `EROFS` (30) as a read-only filesystem does, `EIO` (5) as a failed writeback.
        struct Refuses(Vec<(&'static str, &'static str, i32)>);

        impl Watch for Refuses {
            fn before(&self, call: &Call<'_>) -> io::Result<()> {
                let refused = self.0.iter().find(|(method, suffix, _)| {
                    call.method == *method && call.path().ends_with(suffix)
                });
                match refused {
                    Some(&(_, _, errno)) => Err(io::Error::from_raw_os_error(errno)),
                    None => Ok(()),
                }
            }
        }

        /// The real filesystem, but that it refuses the calls `refused` lists ([`Refuses`]).
        fn refusing(refused: &[(&'static str, &'static str, i32)]) -> Watched<Refuses> {
            Watched(Refuses(refused.to_vec()), RealFs::new())
        }

        /// A store at `<dir>/store`, made, and the identity that commits to it.
        fn new_store(dir: &tempfile::TempDir) -> (Utf8PathBuf, Identity) {
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            drop(LocalOps::init_as(&root, "acme", Some(1)).unwrap());
            (root, Identity::parse("agent/worker-17").unwrap())
        }

        /// A request that puts `path` on main's head.
        fn put(path: &str) -> CommitReq {
            let mut change_input = ChangeInput::new(path.to_owned(), ChangeOp::Put);
            change_input.content = Some(ContentInput::Inline("x".to_owned()));
            CommitReq::new(vec![change_input], "m".to_owned())
        }

        /// `error` is the non-retryable `E_IO` of a journal append to `store_file` that
        /// committed nothing, its hint saying to do `fix` first.
        #[track_caller]
        fn refused(error: &OpError, store_file: &str, fix: &str) {
            assert_eq!(
                (error.code.as_str(), error.exit, error.retryable),
                ("E_IO", 1, false),
                "{error:?}"
            );
            assert_eq!(
                Value::Object(error.details.clone()),
                json!({"committed": false, "fs_op": "append_durable", "store_file": store_file}),
                "{error:?}"
            );
            let hint = error.hint.as_deref().unwrap_or_default();
            assert!(hint.contains(fix), "{error:?}");
        }

        /// (a) `journal.log` refuses the commit's own frame (`EACCES`).
        #[test]
        fn a_frame_append_refused_for_want_of_permission_is_e_io() {
            let dir = tempfile::tempdir().unwrap();
            let (root, who) = new_store(&dir);
            let ops = ops_on(&root, refusing(&[("append_durable", "journal.log", 13)]));
            let error = ops.commit(&who, put("f")).unwrap_err();
            refused(&error, "meta/journal.log", "writable");
            drop(ops);

            let fresh = LocalOps::open(&root).unwrap();
            assert_eq!(fresh.repo_info(InfoReq::default()).unwrap().changesets, 0);
            fresh.commit(&who, put("f")).unwrap();
        }

        /// (b) `journal.off` refuses the tail check's listing of a frame an earlier commit left
        /// whole and unlisted (that commit's own listing failed: committed, with a warning), on a
        /// read-only filesystem (`EROFS`).
        #[test]
        fn a_listing_refused_by_a_read_only_filesystem_is_e_io() {
            let dir = tempfile::tempdir().unwrap();
            let (root, who) = new_store(&dir);
            let ops = ops_on(&root, failing("append_durable", "journal.off"));
            one_injected(&ops.commit(&who, put("f")).unwrap().warnings);
            drop(ops);
            let ops = ops_on(&root, refusing(&[("append_durable", "journal.off", 30)]));
            let error = ops.commit(&who, put("g")).unwrap_err();
            refused(&error, "meta/journal.off", "read-write");
            drop(ops);

            let fresh = LocalOps::open(&root).unwrap();
            fresh.commit(&who, put("g")).unwrap();
            assert_eq!(fresh.repo_info(InfoReq::default()).unwrap().changesets, 2);
        }

        /// (c) The tail check's sync of such a frame fails (`EIO`), and `journal.log` refuses
        /// the rewrite of the frame (`EACCES`): cut back off, the frame is lost (a double fault,
        /// storage spec §2 rule 3), and the request committed nothing.
        #[test]
        fn a_rewrite_refused_for_want_of_permission_is_e_io() {
            let dir = tempfile::tempdir().unwrap();
            let (root, who) = new_store(&dir);
            let ops = ops_on(&root, failing("append_durable", "journal.off"));
            one_injected(&ops.commit(&who, put("f")).unwrap().warnings);
            drop(ops);
            let refusals = [
                ("sync_data", "journal.log", 5),
                ("append_durable", "journal.log", 13),
            ];
            let ops = ops_on(&root, refusing(&refusals));
            let error = ops.commit(&who, put("g")).unwrap_err();
            refused(&error, "meta/journal.log", "writable");
            assert!(error.message.contains("writing afresh"), "{error:?}");
            drop(ops);

            let fresh = LocalOps::open(&root).unwrap();
            fresh.commit(&who, put("g")).unwrap();
            assert_eq!(fresh.repo_info(InfoReq::default()).unwrap().changesets, 1);
        }
    }

    /// A commit whose journal frame cannot be appended (journal.log refuses it) committed
    /// nothing: `E_IO_TRANSIENT`, retryable, `committed: false`, with a hint to run it again. A
    /// fresh handle sees no changeset.
    #[test]
    fn a_commit_whose_journal_append_fails_is_retryable_and_not_committed() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        drop(LocalOps::init_as(&root, "acme", Some(1)).unwrap());
        let who = Identity::parse("agent/worker-17").unwrap();
        let ops = ops_on(&root, failing("append_durable", "journal.log"));
        let mut put = ChangeInput::new("f".to_owned(), ChangeOp::Put);
        put.content = Some(ContentInput::Inline("x".to_owned()));
        let req = CommitReq::new(vec![put], "m".to_owned());
        let error = ops.commit(&who, req).unwrap_err();
        assert_eq!(
            (error.code.as_str(), error.exit, error.retryable),
            ("E_IO_TRANSIENT", 4, true),
            "{error:?}"
        );
        assert_eq!(error.details["committed"], false, "{error:?}");
        assert!(error.hint.is_some(), "{error:?}");
        drop(ops);

        let fresh = LocalOps::open(&root).unwrap();
        assert_eq!(fresh.repo_info(InfoReq::default()).unwrap().changesets, 0);
    }

    /// A commit whose write cannot list the frame a commit before it left whole and unlisted (that
    /// commit's own listing in journal.off failed: committed, with a warning) committed nothing
    /// of its own: `E_IO_TRANSIENT`, retryable, `committed: false`, naming the append that failed,
    /// where it was the non-retryable `E_IO`. Run again where journal.off takes the listing, it
    /// commits, and the store holds both changesets.
    #[test]
    fn a_commit_whose_tail_check_cannot_list_a_frame_is_retryable_and_not_committed() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        drop(LocalOps::init_as(&root, "acme", Some(1)).unwrap());
        let who = Identity::parse("agent/worker-17").unwrap();
        let ops = ops_on(&root, failing("append_durable", "journal.off"));
        let commit = |path: &str| {
            let mut change_input = ChangeInput::new(path.to_owned(), ChangeOp::Put);
            change_input.content = Some(ContentInput::Inline("x".to_owned()));
            CommitReq::new(vec![change_input], "m".to_owned())
        };
        one_injected(&ops.commit(&who, commit("f")).unwrap().warnings);
        let error = ops.commit(&who, commit("g")).unwrap_err();
        assert_eq!(
            (error.code.as_str(), error.exit, error.retryable),
            ("E_IO_TRANSIENT", 4, true),
            "{error:?}"
        );
        assert_eq!(error.details["committed"], false, "{error:?}");
        assert_eq!(error.details["fs_op"], "append_durable", "{error:?}");
        assert_eq!(error.details["store_file"], "meta/journal.off", "{error:?}");
        drop(ops);

        let fresh = LocalOps::open(&root).unwrap();
        fresh.commit(&who, commit("g")).unwrap();
        assert_eq!(fresh.repo_info(InfoReq::default()).unwrap().changesets, 2);
    }

    /// What a request's write repaired first goes into that request's record alone, which the
    /// CLI and the MCP server report as `meta.recovered`, on `E_CAS_MISMATCH` too: after a ref
    /// move whose listing in `journal.off` failed, a mismatching `ref_set`'s write lists the
    /// frame and writes the ref file, and records that; another request on the same `LocalOps`,
    /// whose write repairs nothing, records nothing.
    #[test]
    fn a_request_records_what_its_own_write_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let (ops, _) = LocalOps::init_as(&root, "acme", Some(1)).unwrap();
        let who = Identity::parse("agent/worker-17").unwrap();
        let commit = |content: &str| {
            let mut put = ChangeInput::new("f".to_owned(), ChangeOp::Put);
            put.content = Some(ContentInput::Inline(content.to_owned()));
            let req = CommitReq::new(vec![put], "m".to_owned());
            ops.commit(&who, req).unwrap().changeset
        };
        let (c1, c2) = (commit("1"), commit("2"));
        let set = |new: &str, expected_old: Option<&str>| {
            let mut ref_set_req = RefSetReq::new("main".to_owned(), new);
            ref_set_req.expected_old = expected_old.map(Rev::from);
            ref_set_req.create = expected_old.is_none();
            ref_set_req
        };
        let created = ops_on(&root, failing("append_durable", "journal.off"))
            .ref_set(&who, set(&c1, None))
            .unwrap();
        one_injected(&created.warnings);

        let (first, second) = (Resolved::default(), Resolved::default());
        // main holds c1, not c2.
        let stale = ops
            .recording(&first)
            .ref_set(&who, set(&c2, Some(&c2)))
            .unwrap_err();
        assert_eq!(stale.code.as_str(), "E_CAS_MISMATCH", "{stale:?}");
        let mut listed_and_written = RecoveredOut::default();
        listed_and_written.frames_listed = 1;
        listed_and_written.refs_replayed = 1;
        assert_eq!(first.take_recovered(), Some(listed_and_written));
        assert_eq!(first.take_recovered(), None, "taking it empties the record");
        let replay = ops.recording(&second).ref_set(&who, set(&c1, Some(&c1)));
        assert!(replay.unwrap().already_applied);
        assert_eq!(second.take_recovered(), None, "its write repaired nothing");
    }

    /// A `ref_delete` records what its write repaired first too, whether it deletes the ref or
    /// finds it at another target (`E_CAS_MISMATCH`): after a ref move whose listing in
    /// `journal.off` failed, the delete's write lists the frame and writes the ref file before it
    /// compares.
    #[test]
    fn a_ref_delete_records_what_its_own_write_repaired() {
        let who = Identity::parse("agent/worker-17").unwrap();
        for deletes in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            let (ops, _) = LocalOps::init_as(&root, "acme", Some(1)).unwrap();
            let commit = |content: &str| {
                let mut put = ChangeInput::new("f".to_owned(), ChangeOp::Put);
                put.content = Some(ContentInput::Inline(content.to_owned()));
                let req = CommitReq::new(vec![put], "m".to_owned());
                ops.commit(&who, req).unwrap().changeset
            };
            let (c1, c2) = (commit("1"), commit("2"));
            let mut release = RefSetReq::new("release/1".to_owned(), c1.as_str());
            release.create = true;
            let created = ops_on(&root, failing("append_durable", "journal.off"))
                .ref_set(&who, release)
                .unwrap();
            one_injected(&created.warnings);

            let resolved = Resolved::default();
            // release/1 holds c1: the delete expects it, or c2.
            let expected = if deletes { &c1 } else { &c2 };
            let delete = RefDeleteReq::new("release/1".to_owned(), expected.as_str());
            match ops.recording(&resolved).ref_delete(&who, delete) {
                Ok(deleted) => assert!(deletes && !deleted.already_applied, "{deleted:?}"),
                Err(e) => assert!(!deletes && e.code == "E_CAS_MISMATCH", "{e:?}"),
            }
            let mut listed_and_written = RecoveredOut::default();
            listed_and_written.frames_listed = 1;
            listed_and_written.refs_replayed = 1;
            let recovered = resolved.take_recovered();
            assert_eq!(recovered, Some(listed_and_written), "deletes: {deletes}");
        }
    }

    /// `meta.recovered` gives each count of the store's report in its own field: eight distinct
    /// values, the cut's two among them, each come back where they were.
    #[test]
    fn recovered_out_keeps_each_count_in_its_own_field() {
        let mut recovered = Recovered::default();
        recovered.log_cut = Some(LogCut::new(1, 2));
        recovered.frames_listed = 3;
        recovered.frames_rewritten = 4;
        recovered.off_bytes_dropped = 5;
        recovered.index_records_dropped = 6;
        recovered.views_rebuilt = true;
        recovered.records_replayed = 7;
        recovered.refs_replayed = 8;
        let mut out = RecoveredOut::default();
        out.log_cut = Some(LogCutOut::new(1, 2));
        out.frames_listed = 3;
        out.frames_rewritten = 4;
        out.off_bytes_dropped = 5;
        out.index_records_dropped = 6;
        out.views_rebuilt = true;
        out.records_replayed = 7;
        out.refs_replayed = 8;
        assert_eq!(recovered_out(recovered), out);
    }

    /// A commit with `advance` whose ref moved first is `E_CAS_MISMATCH`, which records what its
    /// write repaired first, as a ref move's does: after a ref move whose listing in
    /// `journal.off` failed, the losing commit's write lists the frame and writes the ref file.
    #[test]
    fn an_advancing_commit_that_loses_records_what_its_write_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let (ops, _) = LocalOps::init_as(&root, "acme", Some(1)).unwrap();
        let who = Identity::parse("agent/worker-17").unwrap();
        let req = |parent: Option<&str>, content: &str, advance: Option<&str>| {
            let mut change_input = ChangeInput::new("f".to_owned(), ChangeOp::Put);
            change_input.content = Some(ContentInput::Inline(content.to_owned()));
            let mut commit_req = CommitReq::new(vec![change_input], "m".to_owned());
            commit_req.parent = parent.map(Rev::from);
            commit_req.advance = advance.map(str::to_owned);
            commit_req
        };
        let c0 = ops.commit(&who, req(None, "0", None)).unwrap().changeset;
        let c1 = ops.commit(&who, req(None, "1", None)).unwrap().changeset;
        let mut main = RefSetReq::new("main".to_owned(), c1.as_str());
        main.create = true;
        let created = ops_on(&root, failing("append_durable", "journal.off"))
            .ref_set(&who, main)
            .unwrap();
        one_injected(&created.warnings);

        let resolved = Resolved::default();
        // main holds c1, not c0.
        let lost = ops
            .recording(&resolved)
            .commit(&who, req(Some(&c0), "2", Some("main")))
            .unwrap_err();
        assert_eq!(lost.code.as_str(), "E_CAS_MISMATCH", "{lost:?}");
        assert_eq!(lost.details["committed"], false, "{lost:?}");
        let mut listed_and_written = RecoveredOut::default();
        listed_and_written.frames_listed = 1;
        listed_and_written.refs_replayed = 1;
        assert_eq!(resolved.take_recovered(), Some(listed_and_written));
        assert_eq!(ops.repo_info(InfoReq::default()).unwrap().changesets, 2);
    }

    /// A keyed commit replays the changeset that carries its key, its ref's head here, only
    /// once that is decided under the lock, after its write's tail check, as a ref move's
    /// replay is (operations spec §4): never from a snapshot, which shows a whole frame
    /// `journal.off` does not list as if it were committed, though its writer may not have
    /// synced it. The first attempt's frame, its changeset and the move of `main` to it, is
    /// whole and durable in journal.log, and its listing in journal.off fails: committed with a
    /// warning, and readers see `main` at it. (a) The retry lists the frame before it answers its
    /// replay, reports that as what its write repaired, and writes no frame of its own. (b)
    /// Where the tail check can neither sync the frame nor write it afresh, it cuts it, and the
    /// retry is a retryable `E_IO_TRANSIENT` that committed nothing, not a replay of a changeset
    /// no reader sees afterwards.
    #[test]
    fn a_keyed_replay_is_decided_after_the_tail_check() {
        // A uuid v7.
        const KEY: &str = "00e8d4a5-13e7-7a1c-9b2d-3e4f5a6b7c8d";
        for tail_check_fails in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            let (ops, _) = LocalOps::init_as(&root, "acme", Some(1)).unwrap();
            let who = Identity::parse("agent/worker-17").unwrap();
            // On main's head, advancing main.
            let publish = |content: &str, key: Option<&str>| {
                let mut change_input = ChangeInput::new("f".to_owned(), ChangeOp::Put);
                change_input.content = Some(ContentInput::Inline(content.to_owned()));
                let mut commit_req = CommitReq::new(vec![change_input], "m".to_owned());
                commit_req.advance = Some("main".to_owned());
                commit_req.idempotency_key = key.map(str::to_owned);
                commit_req
            };
            let c1 = ops.commit(&who, publish("1", None)).unwrap().changeset;
            let first = ops_on(&root, failing("append_durable", "journal.off"))
                .commit(&who, publish("2", Some(KEY)))
                .unwrap();
            one_injected(&first.warnings);
            let c2 = first.changeset;
            let main = |ops: &LocalOps| ops.repo_info(InfoReq::default()).unwrap().main;
            assert_eq!(main(&ops).as_deref(), Some(c2.as_str()), "readers see c2");
            let (log, off) = (root.join("meta/journal.log"), root.join("meta/journal.off"));
            let lens = || (file_len(&log), file_len(&off));
            let (log_len, off_len) = lens();
            if tail_check_fails {
                // The tail check's sync of journal.log fails, and so does its rewrite.
                let model = Arc::new(FsyncModelFs::new().failing(|call| {
                    call.path.ends_with("journal.log")
                        && (call.method == "sync_data" || call.method == "append_durable")
                }));
                let error = ops_on(&root, Arc::clone(&model))
                    .commit(&who, publish("2", Some(KEY)))
                    .unwrap_err();
                assert_eq!(
                    (error.code.as_str(), error.exit, error.retryable),
                    ("E_IO_TRANSIENT", 4, true),
                    "{error:?}"
                );
                assert_eq!(error.details["committed"], false, "{error:?}");
                assert_eq!(
                    model.failed(),
                    [format!("sync_data {log}"), format!("append_durable {log}")]
                );
                let fresh = LocalOps::open(&root).unwrap();
                assert_eq!(
                    main(&fresh).as_deref(),
                    Some(c1.as_str()),
                    "the cut took the commit back"
                );
                assert_eq!(fresh.repo_info(InfoReq::default()).unwrap().changesets, 1);
            } else {
                let resolved = Resolved::default();
                let replay = ops
                    .recording(&resolved)
                    .commit(&who, publish("2", Some(KEY)))
                    .unwrap();
                assert!(replay.replayed && replay.warnings.is_empty(), "{replay:?}");
                assert_eq!(replay.changeset, c2);
                assert!(replay.advanced.is_some_and(|moved| moved.already_applied));
                assert_eq!(
                    lens(),
                    (log_len, off_len + 8),
                    "its write listed the frame before it answered, and wrote no frame of its own"
                );
                // It listed the frame, and wrote the index's record and the ref's file.
                let mut listed_and_written = RecoveredOut::default();
                listed_and_written.frames_listed = 1;
                listed_and_written.records_replayed = 1;
                listed_and_written.refs_replayed = 1;
                assert_eq!(resolved.take_recovered(), Some(listed_and_written));
            }
        }
    }

    /// A `ref_set` to the target it expects is decided under the lock, after its write's tail
    /// check, as every `ref_set` is (operations spec §4): never from a snapshot, which shows a
    /// whole frame `journal.off` does not list as if it were committed, though its writer may not
    /// have synced it. `main` moves from c1 to c2 in a frame whose listing fails: committed with a
    /// warning, and readers see c2. (a) The self-move to c2 lists the frame before it answers its
    /// replay, and writes no frame of its own. (b) Where the tail check can neither sync the frame
    /// nor write it afresh, it cuts it, and the self-move is a retryable `E_IO_TRANSIENT` that
    /// committed nothing, not a replay of a move no reader sees afterwards.
    #[test]
    fn a_move_to_the_target_it_expects_is_decided_after_the_tail_check() {
        for tail_check_fails in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            let (ops, _) = LocalOps::init_as(&root, "acme", Some(1)).unwrap();
            let who = Identity::parse("agent/worker-17").unwrap();
            let commit = |parent: Option<&str>, content: &str| {
                let mut put = ChangeInput::new("f".to_owned(), ChangeOp::Put);
                put.content = Some(ContentInput::Inline(content.to_owned()));
                let mut req = CommitReq::new(vec![put], "m".to_owned());
                req.parent = parent.map(Rev::from);
                ops.commit(&who, req).unwrap().changeset
            };
            let c1 = commit(None, "1");
            let c2 = commit(Some(&c1), "2");
            let set = |new: &str, expected_old: Option<&str>| {
                let mut ref_set_req = RefSetReq::new("main".to_owned(), new);
                ref_set_req.expected_old = expected_old.map(Rev::from);
                ref_set_req.create = expected_old.is_none();
                ref_set_req
            };
            ops.ref_set(&who, set(&c1, None)).unwrap();
            // The move to c2: its frame is whole and durable in journal.log, and its listing in
            // journal.off fails, so readers overlay it, and the next write's tail check must
            // sync and list it.
            let moved = ops_on(&root, failing("append_durable", "journal.off"))
                .ref_set(&who, set(&c2, Some(&c1)))
                .unwrap();
            one_injected(&moved.warnings);
            let main = |ops: &LocalOps| ops.repo_info(InfoReq::default()).unwrap().main;
            assert_eq!(main(&ops).as_deref(), Some(c2.as_str()), "readers see c2");
            let (log, off) = (root.join("meta/journal.log"), root.join("meta/journal.off"));
            let lens = || (file_len(&log), file_len(&off));
            let (log_len, off_len) = lens();
            let self_move = set(&c2, Some(&c2));
            if tail_check_fails {
                // The tail check's sync of journal.log fails, and so does its rewrite.
                let model = Arc::new(FsyncModelFs::new().failing(|call| {
                    call.path.ends_with("journal.log")
                        && (call.method == "sync_data" || call.method == "append_durable")
                }));
                let error = ops_on(&root, Arc::clone(&model))
                    .ref_set(&who, self_move)
                    .unwrap_err();
                assert_eq!(
                    (error.code.as_str(), error.exit, error.retryable),
                    ("E_IO_TRANSIENT", 4, true),
                    "{error:?}"
                );
                assert_eq!(error.details["committed"], false, "{error:?}");
                assert_eq!(
                    model.failed(),
                    [format!("sync_data {log}"), format!("append_durable {log}")]
                );
                let fresh = LocalOps::open(&root).unwrap();
                assert_eq!(
                    main(&fresh).as_deref(),
                    Some(c1.as_str()),
                    "the cut took the move back"
                );
            } else {
                let replay = ops.ref_set(&who, self_move).unwrap();
                assert!(
                    replay.already_applied && replay.warnings.is_empty(),
                    "{replay:?}"
                );
                assert_eq!(
                    lens(),
                    (log_len, off_len + 8),
                    "its write listed the frame before it answered, and wrote no frame of its own"
                );
            }
        }
    }

    /// One snapshot per request (storage spec §8, operations spec §4).
    mod snapshot {
        use swarmops_store::MetaStore;
        use swarmops_testkit::fs::RecordingFs;

        use super::*;

        /// `read_file` of `path` at `main`, 1 KiB at most.
        fn cat(path: &str) -> ReadFileReq {
            let mut read_file_req = ReadFileReq::new("main", path.to_owned());
            read_file_req.max_bytes = 1024;
            read_file_req
        }

        /// `read_tree` of `path` at `main`, 100 entries at most.
        fn ls(path: &str) -> ReadTreeReq {
            let mut read_tree_req = ReadTreeReq::new("main");
            read_tree_req.path = Some(path.to_owned());
            read_tree_req.depth = 1;
            read_tree_req.limit = 100;
            read_tree_req
        }

        /// Each request reads in one snapshot of the metadata, counted by the reads of a witness
        /// ref, `release/witness`, which no request names or moves: every snapshot reads every
        /// ref file once, while a write reads only the refs it moves (its `cas_ref`, and its
        /// replay of the frames past the checkpoint, which the witness's frame is not among once
        /// the store is flushed after it). Exactly one, then, for `read_file` (a file, and
        /// nothing there), `read_tree` (a directory, and a file there), `show`, `log` (from a
        /// revision, and after a cursor), `commit` with a parent, `commit` with `advance` and no
        /// parent, and `ref_set` both moving and replaying; and none for opening the store.
        #[test]
        fn each_request_takes_one_snapshot() {
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            let (setup, _) = LocalOps::init_as(&root, "acme", Some(1)).unwrap();
            let who = Identity::parse("agent/worker-17").unwrap();
            let commit = |parent: Option<&str>, content: &str, advance: Option<&str>| {
                let mut change_input = ChangeInput::new("src/f".to_owned(), ChangeOp::Put);
                change_input.content = Some(ContentInput::Inline(content.to_owned()));
                let mut commit_req = CommitReq::new(vec![change_input], "m".to_owned());
                commit_req.parent = parent.map(Rev::from);
                commit_req.advance = advance.map(str::to_owned);
                commit_req
            };
            let set = |new: &str, expected_old: Option<&str>| {
                let mut ref_set_req = RefSetReq::new("main".to_owned(), new);
                ref_set_req.expected_old = expected_old.map(Rev::from);
                ref_set_req.create = expected_old.is_none();
                ref_set_req
            };
            let c1 = setup
                .commit(&who, commit(None, "1", None))
                .unwrap()
                .changeset;
            setup.ref_set(&who, set(&c1, None)).unwrap();
            let mut witness = set(&c1, None);
            witness.name = "release/witness".to_owned();
            setup.ref_set(&who, witness).unwrap();
            drop(setup);
            // The frames so far checkpointed: no write replays the witness's move again.
            MetaStore::open(&root).unwrap().flush().unwrap();

            let (fs, calls) = RecordingFs::new();
            let ops = ops_on(&root, fs.naming_paths_in(root.clone().into_std_path_buf()));
            let mut counts: Vec<(&str, usize)> = Vec::new();
            let mut count = |request: &'static str| {
                let witness = "read meta/refs/release/witness";
                let reads = calls().into_iter().filter(|call| call == witness).count();
                counts.push((request, reads));
            };
            let log = |rev: Option<&str>, cursor: Option<&str>| {
                let mut log_req = LogReq::default();
                log_req.rev = rev.map(Rev::from);
                log_req.limit = 1;
                log_req.cursor = cursor.map(str::to_owned);
                log_req.paths = Vec::new();
                log_req
            };
            count("open");

            ops.read_file(cat("src/f")).unwrap();
            count("read_file");
            ops.read_file(cat("src/nothing")).unwrap_err();
            count("read_file, nothing there");
            ops.read_tree(ls("src")).unwrap();
            count("read_tree");
            ops.read_tree(ls("src/f")).unwrap_err();
            count("read_tree, a file there");
            let mut show = ShowReq::new("main");
            show.max_changes = 10;
            ops.show(show).unwrap();
            count("show");
            ops.log(log(Some("main"), None)).unwrap();
            count("log");
            let c3 = ops.commit(&who, commit(None, "3", Some("main"))).unwrap();
            count("commit with advance and no parent");
            // A child of main's head: the ref_set below moves main forward.
            let c2 = ops
                .commit(&who, commit(Some(&c3.changeset), "2", None))
                .unwrap();
            count("commit with a parent");
            let (c2, c3) = (c2.changeset, c3.changeset);
            ops.log(log(None, Some(&c2))).unwrap();
            count("log after a cursor");
            assert!(
                !ops.ref_set(&who, set(&c2, Some(&c3)))
                    .unwrap()
                    .already_applied
            );
            count("ref_set");
            assert!(
                ops.ref_set(&who, set(&c2, Some(&c2)))
                    .unwrap()
                    .already_applied
            );
            count("ref_set, a replay");

            let one_each: Vec<(&str, usize)> = (counts.iter())
                .map(|&(request, _)| (request, usize::from(request != "open")))
                .collect();
            assert_eq!(counts, one_each, "snapshots per request");
        }

        /// The history reads take one snapshot each, counted as in
        /// [`each_request_takes_one_snapshot`]: `path_history` from `main`, after a cursor, and of a
        /// path nothing ever touched, and `log` with paths; whatever values of `LastModified` they
        /// derive and store on the way.
        #[test]
        fn history_reads_take_one_snapshot_each() {
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            let (setup, _) = LocalOps::init_as(&root, "acme", Some(1)).unwrap();
            let who = Identity::parse("agent/worker-17").unwrap();
            let mut head: Option<String> = None;
            for content in ["1", "2"] {
                let mut change_input = ChangeInput::new("src/f".to_owned(), ChangeOp::Put);
                change_input.content = Some(ContentInput::Inline(content.to_owned()));
                let mut req = CommitReq::new(vec![change_input], "m".to_owned());
                req.parent = head.as_deref().map(Rev::from);
                req.advance = Some("main".to_owned());
                head = Some(setup.commit(&who, req).unwrap().changeset);
            }
            let mut witness = RefSetReq::new("release/witness".to_owned(), head.unwrap().as_str());
            witness.create = true;
            setup.ref_set(&who, witness).unwrap();
            drop(setup);
            // The frames so far checkpointed: no write replays the witness's move again.
            MetaStore::open(&root).unwrap().flush().unwrap();

            let (fs, calls) = RecordingFs::new();
            let ops = ops_on(&root, fs.naming_paths_in(root.clone().into_std_path_buf()));
            let witness_reads = || {
                let witness = "read meta/refs/release/witness";
                calls().into_iter().filter(|call| call == witness).count()
            };
            assert_eq!(witness_reads(), 0, "opening the store");
            let history = |cursor: Option<&str>, path: &str| {
                let mut path_history_req = PathHistoryReq::new(path.to_owned());
                path_history_req.limit = 1;
                path_history_req.cursor = cursor.map(str::to_owned);
                path_history_req
            };
            let page = ops.path_history(history(None, "src")).unwrap();
            assert_eq!(witness_reads(), 1, "path_history");
            let rest = ops.path_history(history(page.cursor.as_deref(), "src"));
            assert_eq!(rest.unwrap().entries.len(), 1);
            assert_eq!(witness_reads(), 1, "path_history after a cursor");
            ops.path_history(history(None, "nothing")).unwrap_err();
            assert_eq!(witness_reads(), 1, "path_history, nothing ever there");
            let mut log = LogReq::default();
            log.rev = None;
            log.limit = 1;
            log.cursor = None;
            log.paths = vec!["src".to_owned()];
            ops.log(log).unwrap();
            assert_eq!(witness_reads(), 1, "log with paths");
        }
        /// A diff resolves both its revisions in one snapshot (operations spec §4), the one all
        /// its reads are made in: the witness ref is read once, and the request records both
        /// revisions as resolved.
        #[test]
        fn diff_resolves_both_revisions_in_one_snapshot() {
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            let (setup, _) = LocalOps::init_as(&root, "acme", Some(1)).unwrap();
            let who = Identity::parse("agent/worker-17").unwrap();
            let commit = |parent: Option<&str>, content: &str| {
                let mut change_input = ChangeInput::new("src/f".to_owned(), ChangeOp::Put);
                change_input.content = Some(ContentInput::Inline(content.to_owned()));
                let mut req = CommitReq::new(vec![change_input], "m".to_owned());
                req.parent = parent.map(Rev::from);
                req.advance = Some("main".to_owned());
                setup.commit(&who, req).unwrap().changeset
            };
            let c1 = commit(None, "1");
            let c2 = commit(Some(&c1), "2");
            let mut witness = RefSetReq::new("release/witness".to_owned(), c1.as_str());
            witness.create = true;
            setup.ref_set(&who, witness).unwrap();
            drop(setup);
            MetaStore::open(&root).unwrap().flush().unwrap();

            let (fs, calls) = RecordingFs::new();
            let ops = ops_on(&root, fs.naming_paths_in(root.clone().into_std_path_buf()));
            let resolved = Resolved::default();
            let mut req = DiffReq::new("main~1", "main");
            req.format = DiffFormat::Unified;
            req.context = 3;
            req.limit = 500;
            req.max_bytes = 32_768;
            let page = ops.recording(&resolved).diff(req).unwrap();
            let witness = "read meta/refs/release/witness";
            let snapshots = calls().into_iter().filter(|call| call == witness).count();
            assert_eq!(snapshots, 1, "snapshots of one diff");
            assert_eq!(
                resolved.take(),
                BTreeMap::from([
                    ("main".to_owned(), c2.clone()),
                    ("main~1".to_owned(), c1.clone()),
                ])
            );
            assert_eq!((page.from, page.to, page.files.len()), (c1, c2, 1));
        }
    }

    /// A diff reads no blob past the line diff's bound (`MAX_DIFF_BYTES`).
    mod bounded_diff {
        use swarmops_repo::merge::line_diff::MAX_DIFF_BYTES;
        use swarmops_store::Layout;
        use swarmops_testkit::fs::RecordingFs;
        use swarmops_testkit::repo::blob;

        use super::*;

        /// Text of `MAX_DIFF_BYTES` and a line more, whose first line is `head`.
        fn big_text(head: &str) -> String {
            let line = "0123456789abcde\n";
            let mut text = format!("{head}\n");
            text.extend(std::iter::repeat_n(line, MAX_DIFF_BYTES / line.len() + 1));
            text
        }

        /// A store with `main` at the changeset of `old` (its first) and `new` (its second), the
        /// two put at `f`; reopened on a recorder.
        fn two(
            old: &[u8],
            new: &[u8],
        ) -> (
            tempfile::TempDir,
            Utf8PathBuf,
            impl Fn() -> Vec<String>,
            LocalOps,
        ) {
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            let (setup, _) = LocalOps::init(&root, "acme").unwrap();
            let who = Identity::parse("agent/worker-17").unwrap();
            let commit = |parent: Option<&str>, content: &[u8]| {
                let mut change_input = ChangeInput::new("f".to_owned(), ChangeOp::Put);
                let content = String::from_utf8_lossy(content).into_owned();
                change_input.content = Some(ContentInput::Inline(content));
                let mut req = CommitReq::new(vec![change_input], "m".to_owned());
                req.parent = parent.map(Rev::from);
                req.advance = Some("main".to_owned());
                setup.commit(&who, req).unwrap().changeset
            };
            let c1 = commit(None, old);
            commit(Some(&c1), new);
            drop(setup);
            let (fs, calls) = RecordingFs::new();
            let ops = ops_on(&root, fs.naming_paths_in(root.clone().into_std_path_buf()));
            (dir, root, calls, ops)
        }

        /// The reads of the object files of `contents` among `calls`.
        fn reads_of(calls: &[String], contents: &[&[u8]]) -> usize {
            let wanted: Vec<String> = (contents.iter())
                .map(|content| format!("read {}", Layout::new("").object_path(blob(content).0)))
                .collect();
            calls.iter().filter(|call| wanted.contains(call)).count()
        }

        fn diff_of(ops: &LocalOps, format: DiffFormat) -> Diff {
            let mut req = DiffReq::new("main~1", "main");
            req.format = format;
            req.context = 3;
            req.limit = 500;
            req.max_bytes = 32_768;
            ops.diff(req).unwrap()
        }

        /// Both sides past the bound: `stat` and `unified` give the page they gave when the blobs
        /// were read (text, no line counts, hunks cut), and read neither blob, only its header.
        #[test]
        fn blobs_past_the_bound_are_not_read() {
            let (old, new) = (big_text("a"), big_text("b"));
            let (_dir, _root, calls, ops) = two(old.as_bytes(), new.as_bytes());
            calls();
            for format in [DiffFormat::Stat, DiffFormat::Unified] {
                let page = diff_of(&ops, format);
                let file = &page.files[0];
                assert_eq!(
                    (
                        file.binary,
                        file.insertions,
                        file.deletions,
                        file.hunks_truncated
                    ),
                    (Some(false), None, None, Some(true)),
                    "{format:?}"
                );
                let hunks = file.hunks.as_ref().map(Vec::len);
                assert_eq!(hunks, (format == DiffFormat::Unified).then_some(0));
            }
            let calls = calls();
            assert_eq!(
                reads_of(&calls, &[old.as_bytes(), new.as_bytes()]),
                0,
                "{calls:?}"
            );
        }

        /// A small side that is not text settles the file as binary, as it did: the big side is
        /// still not read.
        #[test]
        fn a_small_binary_side_settles_it_without_the_big_one() {
            let big = big_text("a");
            let (_dir, _root, calls, ops) = two(big.as_bytes(), b"nul\0here");
            calls();
            let page = diff_of(&ops, DiffFormat::Stat);
            assert_eq!(page.files[0].binary, Some(true));
            assert_eq!(page.files[0].hunks_truncated, None);
            assert_eq!(reads_of(&calls(), &[big.as_bytes()]), 0);
        }
    }

    /// `git_import` made through a request's record records what its writes repaired first, as
    /// `commit` does: after a ref move whose listing in `journal.off` failed, the import's first
    /// frame lists that frame and writes the ref file, and the request's record says so, which
    /// the CLI reports as `meta.recovered`; `LocalOps::git_import` itself records nothing.
    #[test]
    fn a_git_import_records_what_its_writes_repaired() {
        use swarmops_api::GitImportReq;

        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let (ops, _) = LocalOps::init_as(&root, "acme", Some(1)).unwrap();
        let who = Identity::parse("agent/worker-17").unwrap();
        let refs = swarmops_testkit::git::fixture("refs");
        let git_path = refs.repo.as_deref().and_then(std::path::Path::to_str);
        let git_path = git_path.expect("a repository at a UTF-8 path").to_owned();
        let import = |git_ref: &str| {
            let mut git_import_req = GitImportReq::new(git_path.clone());
            git_import_req.git_ref = git_ref.to_owned();
            git_import_req.target_ref = "main".to_owned();
            git_import_req
        };
        let older = ops.git_import(&who, import("older")).unwrap();
        let head = ObjectId::from_hex(&older.head_changeset).map(ChangesetId::new);
        let moved = ops_on(&root, failing("append_durable", "journal.off"))
            .repo
            .cas_ref("release/older", None, Some(head.unwrap()), who.agent())
            .unwrap();
        one_injected(&moved.warnings);

        let resolved = Resolved::default();
        let newer = ops.recording(&resolved).git_import(&who, import("newer"));
        let newer = newer.unwrap();
        assert_eq!((newer.imported, newer.skipped), (3, 1), "{newer:?}");
        let mut listed_and_written = RecoveredOut::default();
        listed_and_written.frames_listed = 1;
        listed_and_written.refs_replayed = 1;
        assert_eq!(resolved.take_recovered(), Some(listed_and_written));
    }

    /// `git_export` made through a request's record records the head its source ref named, as a
    /// revision it resolved, and what its committed snapshot's tail check repaired: after a ref
    /// move whose listing in `journal.off` failed, the check lists that frame, and replays nothing
    /// (the snapshot reads the frame), which the CLI reports as `meta.recovered`. Exported again,
    /// the tail is clean, and the record says nothing of repairs.
    #[test]
    fn a_git_export_records_its_source_and_what_its_tail_check_repaired() {
        use swarmops_api::GitExportReq;

        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let (ops, _) = LocalOps::init_as(&root, "acme", Some(1)).unwrap();
        let who = Identity::parse("agent/worker-17").unwrap();
        let mut change_input = ChangeInput::new("src/f".to_owned(), ChangeOp::Put);
        change_input.content = Some(ContentInput::Inline("1".to_owned()));
        let mut req = CommitReq::new(vec![change_input], "m".to_owned());
        req.advance = Some("main".to_owned());
        let head = ops.commit(&who, req).unwrap().changeset;
        let id = ObjectId::from_hex(&head).map(ChangesetId::new).unwrap();
        let moved = ops_on(&root, failing("append_durable", "journal.off"))
            .repo
            .cas_ref("release/x", None, Some(id), who.agent())
            .unwrap();
        one_injected(&moved.warnings);

        let destination = swarmops_testkit::git::empty_repository();
        let git_path = destination.dir.to_str().expect("a UTF-8 path").to_owned();
        let export = || {
            let mut git_export_req = GitExportReq::new(git_path.clone());
            git_export_req.source_ref = "main".to_owned();
            git_export_req.branch = "main".to_owned();
            git_export_req
        };
        let resolved = Resolved::default();
        let exported = ops.recording(&resolved).git_export(export()).unwrap();
        assert_eq!((exported.exported, exported.branch_moved), (1, true));
        assert_eq!(exported.head_changeset, head);
        let mut listed = RecoveredOut::default();
        listed.frames_listed = 1;
        assert_eq!(resolved.take_recovered(), Some(listed));
        assert_eq!(resolved.take(), BTreeMap::from([("main".to_owned(), head)]));

        let again = Resolved::default();
        let up_to_date = ops.recording(&again).git_export(export()).unwrap();
        assert_eq!((up_to_date.exported, up_to_date.branch_moved), (0, false));
        assert_eq!(again.take_recovered(), None);
    }

    /// A land that loses its race at every attempt fails `E_CAS_RETRY_EXHAUSTED` (exit 4,
    /// retryable), naming how many attempts it made and what the target names after the last,
    /// with a hint to run it again; it recorded nothing, and run again it lands. Here the land
    /// makes one attempt, and another lander moves main while it flushes its changeset's objects,
    /// editing the file the land edits, a line apart, past the size the publish merges in the
    /// lock (1 MiB a side), so the publish gives the land back.
    #[test]
    fn retry_exhausted_is_retryable_with_attempts_and_actual() {
        use std::sync::Mutex;

        use swarmops_store::RealFs;
        use swarmops_testkit::fs::{Call, Watch, Watched};

        /// Moves main once, as another lander would: at the land's first flush of objects (its
        /// first `fsync_dir` under `objects/`, outside the lock), it publishes a change on main's
        /// head through a handle of its own on the real filesystem.
        struct Race {
            root: Utf8PathBuf,
            /// What it puts at `registry`.
            content: String,
            /// The changeset it moved main to, once it has.
            raced: Mutex<Option<String>>,
        }

        impl Watch for Race {
            fn before(&self, call: &Call<'_>) -> io::Result<()> {
                let at = call.path().strip_prefix(&self.root).unwrap_or(call.path());
                let mut raced = self.raced.lock().unwrap();
                if call.method == "fsync_dir" && at.starts_with("objects") && raced.is_none() {
                    let other = Identity::parse("agent/worker-9").unwrap();
                    let mut change = ChangeInput::new("registry".to_owned(), ChangeOp::Put);
                    change.content = Some(ContentInput::Inline(self.content.clone()));
                    let mut req = CommitReq::new(vec![change], "a race".to_owned());
                    req.advance = Some("main".to_owned());
                    let won = LocalOps::open(&self.root).unwrap().commit(&other, req);
                    *raced = Some(won.unwrap().changeset);
                }
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
        let (ops, _) = LocalOps::init(&root, "acme").unwrap();
        let who = Identity::parse("agent/worker-17").unwrap();
        // 32,000 lines of 36 bytes, line `at` replaced by `line`.
        let registry = |at: usize, line: &str| -> String {
            let lines = (0..32_000_usize).map(|n| {
                if n == at {
                    format!("{line}\n")
                } else {
                    format!("entry {n:06} of the registry, as is\n")
                }
            });
            lines.collect()
        };
        let commit = |parent: Option<&str>, content: String, advance: Option<&str>| {
            let mut change = ChangeInput::new("registry".to_owned(), ChangeOp::Put);
            change.content = Some(ContentInput::Inline(content));
            let mut req = CommitReq::new(vec![change], "registry".to_owned());
            req.parent = parent.map(Rev::from);
            req.advance = advance.map(str::to_owned);
            ops.commit(&who, req).unwrap().changeset
        };
        let t0 = commit(
            None,
            registry(0, "entry 000000 of the registry, as is"),
            Some("main"),
        );
        let head = commit(Some(&t0), registry(4, "ours"), None);
        let race = Arc::new(Race {
            root: root.clone(),
            content: registry(1, "theirs"),
            raced: Mutex::new(None),
        });
        let racing = ops_on(&root, Watched(Arc::clone(&race), RealFs::new()));
        let land: LandReq =
            serde_json::from_value(json!({"head": head, "max_attempts": 1})).unwrap();
        let frames = || {
            let mut op_log_req = OpLogReq::default();
            op_log_req.before = None;
            op_log_req.limit = 1;
            op_log_req.max_changesets = 1;
            ops.op_log(op_log_req).unwrap().ops[0].op
        };
        let before = frames();

        let error = racing.land(&who, land.clone()).unwrap_err();
        let actual = race.raced.lock().unwrap().clone().expect("the race ran");
        assert_eq!(
            (error.code.as_str(), error.exit, error.retryable),
            ("E_CAS_RETRY_EXHAUSTED", 4, true),
            "{error:?}"
        );
        assert_eq!(
            Value::Object(error.details.clone()),
            json!({"attempts": 1, "actual": actual})
        );
        assert_eq!(
            error.hint.as_deref(),
            Some("run the land again: nothing of this one was recorded")
        );
        let main = || ops.repo_info(InfoReq::default()).unwrap().main;
        assert_eq!(main().as_deref(), Some(actual.as_str()));
        assert_eq!(frames(), before + 1, "the race's frame alone");

        let again = ops.land(&who, land).unwrap();
        assert_eq!(
            (again.status, again.rebased_onto.as_str(), again.attempts),
            (LandStatus::Landed, actual.as_str(), 1)
        );
        assert_eq!(main(), Some(again.landed));
    }

    /// The tests of `rebase` and of `land`'s `expect_target` (M5).
    mod exact {
        use std::io;
        use std::sync::Mutex;

        use camino::Utf8PathBuf;
        use serde_json::{Value, json};
        use swarmops_api::{
            ChangeInput, ChangeOp, ChangesetDetail, CommitReq, ContentInput, Identity, InfoReq,
            LandReq, LandStatus, OpLogReq, RebaseReq, RebaseStatus, Rev, ShowReq, StoreOps,
        };
        use swarmops_store::RealFs;
        use swarmops_testkit::fs::{Call, Watch, Watched};

        use super::ops_on;
        use crate::local::LocalOps;

        /// A uuid v7 whose time is 2023-06-30 03:34:18.518 UTC.
        const KEY: &str = "01890a5d-ac96-774b-bcce-b302099a8057";

        /// The key's time in whole seconds.
        const KEY_SECS: u64 = 1_688_096_058;

        /// A store with `main` at `t0`, an agent's change `head` on it, and `main` moved on to
        /// `t1`; the ops over it and the agent.
        fn moved_on() -> (
            tempfile::TempDir,
            Utf8PathBuf,
            LocalOps,
            Identity,
            [String; 3],
        ) {
            let dir = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::from_path_buf(dir.path().join("store")).unwrap();
            let (ops, _) = LocalOps::init(&root, "acme").unwrap();
            let who = Identity::parse("agent/worker-17").unwrap();
            let t0 = put(&ops, &who, None, "a", Some("main"));
            let head = put(&ops, &who, Some(&t0), "b", None);
            let t1 = put(&ops, &who, Some(&t0), "c", Some("main"));
            (dir, root, ops, who, [t0, head, t1])
        }

        /// Commits a put of `path` on `parent`, advancing `advance`; the changeset's id.
        fn put(
            ops: &LocalOps,
            who: &Identity,
            parent: Option<&str>,
            path: &str,
            advance: Option<&str>,
        ) -> String {
            let mut change_input = ChangeInput::new(path.to_owned(), ChangeOp::Put);
            change_input.content = Some(ContentInput::Inline(format!("{path}\n")));
            let mut req = CommitReq::new(vec![change_input], path.to_owned());
            req.parent = parent.map(Rev::from);
            req.advance = advance.map(str::to_owned);
            ops.commit(who, req).unwrap().changeset
        }

        /// A rebase of `fields` over the defaults.
        fn rebase(fields: Value) -> RebaseReq {
            serde_json::from_value(fields).unwrap()
        }

        /// A land of `fields` over the defaults.
        fn land(fields: Value) -> LandReq {
            serde_json::from_value(fields).unwrap()
        }

        /// `show` of `id`.
        fn show(ops: &LocalOps, id: &str) -> ChangesetDetail {
            let mut req = ShowReq::new(Rev::from(id));
            req.max_changes = 10;
            ops.show(req).unwrap()
        }

        /// A rebase records `H'` on `main`'s head, moving no ref, and a land of it with
        /// `expect_target` set to that head publishes its tree: `L`'s root, read through `show`,
        /// is the rebase's `root`. `H'` is no land: no lander, no land key.
        #[test]
        fn a_land_with_expect_target_publishes_the_rebased_tree() {
            let (_dir, _root, ops, who, [t0, head, t1]) = moved_on();

            // 1. The rebase.
            let rebased = ops.rebase(&who, rebase(json!({"head": head}))).unwrap();
            assert_eq!(rebased.status, RebaseStatus::Rebased);
            assert_eq!(
                (rebased.onto.as_str(), rebased.base.as_str()),
                (t1.as_str(), t0.as_str())
            );
            let shown = show(&ops, &rebased.changeset);
            assert_eq!(
                (shown.parents.clone(), shown.root.clone()),
                (vec![t1.clone()], rebased.root.clone())
            );
            assert_eq!((shown.lander, shown.land_key), (None, None));
            assert_eq!(shown.squashed_from, [head]);
            let main = || ops.repo_info(InfoReq::default()).unwrap().main;
            assert_eq!(main(), Some(t1.clone()));

            // 2. The exact land.
            let fields = json!({"head": rebased.changeset, "base": t1, "expect_target": t1});
            let landed = ops.land(&who, land(fields)).unwrap();
            assert_eq!((landed.status, landed.attempts), (LandStatus::Landed, 1));
            assert_eq!(show(&ops, &landed.landed).root, rebased.root);
            assert_eq!(main(), Some(landed.landed));
        }

        /// A keyed rebase signs at its key's time and records the key as `commit_key`: the same
        /// request again is the same changeset, `replayed`.
        #[test]
        fn a_keyed_rebase_signs_at_its_key_s_time_and_replays() {
            let (_dir, _root, ops, who, [_, head, _]) = moved_on();
            let keyed = || rebase(json!({"head": head, "idempotency_key": KEY}));

            let first = ops.rebase(&who, keyed()).unwrap();
            let shown = show(&ops, &first.changeset);
            assert_eq!(shown.created_at, KEY_SECS * 1000);
            assert_eq!(shown.metadata.get("commit_key"), Some(&KEY.to_owned()));

            let again = ops.rebase(&who, keyed()).unwrap();
            assert_eq!(
                (again.status, again.changeset),
                (RebaseStatus::Replayed, first.changeset)
            );
        }

        /// A rebase's own rules are checked before anything is read: `onto` is `main` or
        /// `release/<x>`, a key is a uuid v7.
        #[test]
        fn a_rebase_checks_its_request_first() {
            let (_dir, _root, ops, who, [_, head, _]) = moved_on();
            let refused = |fields| ops.rebase(&who, rebase(fields)).unwrap_err();

            let onto = refused(json!({"head": head, "onto": "main/x"}));
            assert_eq!(onto.code.as_str(), "E_REF_NAME_INVALID", "{onto:?}");
            let key = refused(json!({"head": head, "idempotency_key": "key-1"}));
            assert_eq!(key.code.as_str(), "E_INVALID_ARG", "{key:?}");
            assert_eq!(
                key.details.get("idempotency_key"),
                Some(&Value::from("key-1"))
            );
        }

        /// A rebase that conflicts is `E_CONFLICT` (exit 3) with the report a land of the same
        /// change gives.
        #[test]
        fn a_rebase_that_conflicts_reports_as_a_land_does() {
            // The change adds `c` with other content than main's `c` on t1.
            let (_dir, _root, ops, who, [t0, _, _]) = moved_on();
            let mut change_input = ChangeInput::new("c".to_owned(), ChangeOp::Put);
            change_input.content = Some(ContentInput::Inline("ours\n".to_owned()));
            let mut edit = CommitReq::new(vec![change_input], "ours".to_owned());
            edit.parent = Some(Rev::from(t0.as_str()));
            let head = ops.commit(&who, edit).unwrap().changeset;

            let rebased = ops.rebase(&who, rebase(json!({"head": head}))).unwrap_err();
            let landed = ops.land(&who, land(json!({"head": head}))).unwrap_err();
            assert_eq!(
                (rebased.code.as_str(), rebased.exit),
                ("E_CONFLICT", 3),
                "{rebased:?}"
            );
            assert_eq!(rebased.details.get("report"), landed.details.get("report"));
        }

        /// A land whose target no longer names `expect_target` is `E_CAS_MISMATCH` (exit 4,
        /// retryable) `{name, actual, expected, committed: false}`, and records nothing.
        #[test]
        fn a_land_onto_a_moved_target_is_a_cas_mismatch() {
            let (_dir, _root, ops, who, [t0, head, t1]) = moved_on();
            let fields = json!({"head": head, "base": t0, "expect_target": t0});

            let error = ops.land(&who, land(fields)).unwrap_err();
            assert_eq!(
                (error.code.as_str(), error.exit, error.retryable),
                ("E_CAS_MISMATCH", 4, true),
                "{error:?}"
            );
            let want = json!({"name": "main", "actual": t1, "expected": t0, "committed": false});
            assert_eq!(Value::Object(error.details.clone()), want);
            assert_eq!(ops.repo_info(InfoReq::default()).unwrap().main, Some(t1));
        }

        /// Moves main once, as another lander would, at the land's first flush of objects
        /// (outside the lock): a commit on main's head through a handle of its own.
        struct Race {
            root: Utf8PathBuf,
            /// The changeset it moved main to, once it has.
            raced: Mutex<Option<String>>,
        }

        impl Watch for Race {
            fn before(&self, call: &Call<'_>) -> io::Result<()> {
                let at = call.path().strip_prefix(&self.root).unwrap_or(call.path());
                let mut raced = self.raced.lock().unwrap();
                if call.method == "fsync_dir" && at.starts_with("objects") && raced.is_none() {
                    let other = Identity::parse("agent/worker-9").unwrap();
                    let ops = LocalOps::open(&self.root).unwrap();
                    *raced = Some(put(&ops, &other, None, "race", Some("main")));
                }
                Ok(())
            }
        }

        /// An exact land that loses its race to the target publishes nothing and merges no second
        /// time: `E_CAS_MISMATCH` naming what the race left, whatever `max_attempts` says; the
        /// journal holds the race's frame alone.
        #[test]
        fn an_exact_land_that_loses_its_race_is_a_cas_mismatch() {
            let (_dir, root, ops, who, [_, head, t1]) = moved_on();
            let rebased = ops.rebase(&who, rebase(json!({"head": head}))).unwrap();
            let race = std::sync::Arc::new(Race {
                root: root.clone(),
                raced: Mutex::new(None),
            });
            let racing = ops_on(&root, Watched(std::sync::Arc::clone(&race), RealFs::new()));
            let frames = || {
                let mut op_log_req = OpLogReq::default();
                op_log_req.before = None;
                op_log_req.limit = 1;
                op_log_req.max_changesets = 1;
                let page = ops.op_log(op_log_req);
                page.unwrap().ops[0].op
            };
            let before = frames();

            let fields = json!({"head": rebased.changeset, "base": t1, "expect_target": t1, "max_attempts": 8});
            let error = racing.land(&who, land(fields)).unwrap_err();
            let actual = race.raced.lock().unwrap().clone().expect("the race ran");
            assert_eq!(error.code.as_str(), "E_CAS_MISMATCH", "{error:?}");
            let want =
                json!({"name": "main", "actual": actual, "expected": t1, "committed": false});
            assert_eq!(Value::Object(error.details.clone()), want);
            assert_eq!(frames(), before + 1, "the race's frame alone");
        }
    }
}
