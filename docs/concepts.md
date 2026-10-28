# Concepts

This page defines the nouns SwarmOps uses, from the store and its objects to refs, landing and the
envelope, and links each to the documents that explain it.

## Store

A store is one directory: `FORMAT` names its layout version, `REPO` the repository's id and name,
`objects/` holds the [objects](#object) and `meta/` the metadata. Every command finds it by `--repo`
or `SWARMOPS_REPO`.

SwarmOps is fully embedded: one program, no external database, no daemon. Each command or
MCP (Model Context Protocol) call opens the store, does its work and closes it.

Explained in: [README, The store on disk](../README.md#the-store-on-disk) ·
[architecture, The store on disk](architecture.md#the-store-on-disk) ·
[operations, Create a store](operations.md#create-a-store).

## Object

An object is immutable and content-addressed: a [blob](#blob), a [tree](#tree) or a
[changeset](#changeset), each one file in `objects/`. Its id is a BLAKE3-256 hash with the object's
kind and layout version hashed in, printed as 64 lowercase hex digits. A blob's id is given in
full; a changeset's may be shortened to a unique prefix of 12 digits or more
([Revision](#revision)).

The byte layouts and the hash recipe are the hash contract: they never change in place, and a new
layout gets a new kind/version header.

Explained in: [architecture, Data model](architecture.md#data-model) ·
[versioning policy, Public surfaces](policies/versioning-and-stability.md#public-surfaces).

### Blob

A blob is a file's content, raw bytes. A symlink's target is a blob too; the executable bit lives in
the tree entry. `blob put` stores a file's content as a blob, outside any commit, and a change names
a stored blob by id.

### Tree

A tree is one directory: entries sorted by the bytes of their names, each a kind (File, Exec,
Symlink or Dir), a name and an id. There are no empty directories; only the root tree may be empty.

### Changeset

A changeset is one commit: its parents (`parents[0]` is the mainline), `root` (the id of the whole
resulting tree), `delta` (what changed against `parents[0]`), git-style author and committer
signatures, the message, `extra_headers` (git headers verbatim and in order) and `metadata`, the
keys SwarmOps owns, such as the committing agent. Changesets are never removed.

## Revision

A revision is `main`, `release/<name>`, a changeset id or a unique prefix of 12 or more of its hex
digits, then any number of `~N` (the N-th first parent). `REV:PATH` names a path at a revision, as
`cat` and `history` take it, and `ls` takes `REV[:PATH]`.

`resolve REV` gives the full id of the changeset a revision names, and an envelope's
`meta.resolved` says what each revision named.

Explained in: [README, Revisions and change lists](../README.md#revisions-and-change-lists) ·
[README, Commands](../README.md#commands).

## Change list

A change list is a JSON array of `{"path", "op": "put" | "delete", "content", "mode"}`: a put's
`content` is `{"inline": "<UTF-8 text>"}`, `{"base64": "<bytes>"}`, `{"file": "<path>"}` (read by
the CLI, on the client) or `{"blob": "<id>"}`, a blob stored before; its `mode` is `file` (the
default), `exec` or `symlink`.

`commit --changes` reads one from a file, or from stdin for `-`; directories follow from the paths.
A commit's contents hold at most 1 GiB together (a blob named by id counts nothing) and a change
list's text at most 64 MiB (`E_TOO_LARGE` past either), and a file's content at most 512 MiB.

Explained in: [README, Revisions and change lists](../README.md#revisions-and-change-lists) ·
[architecture, Data model](architecture.md#data-model).

## Ref

A ref names a changeset. Refs are `main` and `release/<name>` only, each one file under
`meta/refs/`, moved one per call and only by [compare-and-swap](#compare-and-swap). There are no
tags, free-form branch names or symbolic refs; the journal's ref moves are the reflog.

`ref list` lists the refs, and `ref set` and `ref delete` move one.

Explained in: [README, Commands](../README.md#commands) ·
[architecture, Writing](architecture.md#writing).

## Compare-and-swap

Compare-and-swap is how a ref moves: the caller names the target it expects the ref to hold, and the
store compares it with the ref's current target under its lock. On a mismatch nothing is recorded,
and the caller gets `E_CAS_MISMATCH` (exit 4) with the ref's actual target in `details.actual`.

`ref set --expect` or `--create`, `ref delete --expect`, `commit --advance`, `land` and `op undo`
all move refs this way. When several agents expect the same target, exactly one wins each round.

Explained in: [README, Revisions and change lists](../README.md#revisions-and-change-lists) ·
[architecture, Concurrency](architecture.md#concurrency).

## Land

To land a change is to merge it three-way onto a ref's head and publish it there, as one new
changeset in one transaction. `land` merges again and retries by itself when the ref moves
meanwhile, and a land run again with its `--key` replays (`status: replayed`).

A change that conflicts with what the ref gained fails with `E_CONFLICT` (exit 3) and the conflict
report in `details.report`, recording nothing; `land --resolve` then resolves it without a checkout,
path by path. Landing is built, but not yet measured under a swarm's load.

Explained in: [README, First commands](../README.md#first-commands) · [README,
Commands](../README.md#commands) · [architecture, Concurrency](architecture.md#concurrency).

## Envelope

An envelope is the JSON object a command prints on stdout, one per run:
`{ok, op, api, result | error, meta}`, with `api` 1. A failure's `error` carries a stable `E_*`
code, a fixed exit code, whether running it again can succeed (`retryable`), machine-readable
`details` and a `hint`, often the next command to run.

The exit code is `error.exit`: 0 success, 1 error, 2 usage, 3 conflict, 4 retryable, 5 identity or
policy, 6 not found. An MCP tool's result is the same envelope. `derive --follow` prints one
envelope a line, `--help`, `--version` and the `help` subcommand print plain text, and `mcp` speaks
MCP on stdout instead; a run that meets one of the store's four documented aborts prints no
envelope.

Explained in: [README, Output and exit codes](../README.md#output-and-exit-codes) ·
[architecture, The contract](architecture.md#the-contract).

## Identity

An identity says who makes a change: `<family>/<instance>`, such as `agent/worker-1`, from `--as`,
else `SWARMOPS_AGENT`, with `SWARMOPS_RUN_ID` naming the agent's run if it has one. A family is
`[a-z0-9-]`, an instance `[A-Za-z0-9._-]` that does not start with `.`.

`commit`, `land`, `blob put`, `ref set`, `ref delete`, `op undo` and `git import` need one, and fail
with `E_IDENTITY_REQUIRED` (exit 5) without it; `init`, reads, `git export` and `derive` need none.
The changeset records the identity, and its run as `run_id` metadata; the journal records who moved
each ref. SwarmOps records identities without verifying them (`meta.auth` is `"trust"`).

Explained in: [README, Store, identity and lifetime](../README.md#store-identity-and-lifetime) ·
[operations, Identities](operations.md#identities).

## Op log and undo

The store's operations are its write transactions: every commit, publish and ref move is one,
numbered in commit order (`op`). `op log` lists them, newest first, each with its kind, its
changesets and its ref moves: the [journal](#journal-and-frame), shown a frame each.

`op undo <op>` moves every ref that operation moved back to what it named before, all of them or
none, by compare-and-swap; if another operation has moved one of them since, it refuses with
`E_CAS_MISMATCH` and writes nothing. An undo is an operation of its own, which another undo takes
back.

Explained in: [README, First commands](../README.md#first-commands) ·
[architecture, Concurrency](architecture.md#concurrency).

## Journal and frame

The journal is `meta/journal.log`: append-only and CRC-framed, one frame per write transaction, and
the source of truth of the store's metadata. A frame is
`u32le len | u32le crc32(payload) | payload`, and its payload holds fixed-width little-endian
records: a changeset entering the index, a ref move, a compaction marker.

A write commits when its journal frame is durable, at the frame's `fdatasync`, the commit point, so
the next write repairs whatever a crash cut short. The index and the ref files are projections of
the journal.

Explained in: [README, The store on disk](../README.md#the-store-on-disk) ·
[architecture, The store on disk](architecture.md#the-store-on-disk) ·
[architecture, Writing](architecture.md#writing).

## Derived value

A derived value is computed from immutable objects and cached in a file of its own under
`meta/derived/`: each changeset's first-parent skip list, say, or the last changeset to change a
path.

Nothing is synced and no lock is taken: no journal record or other file names a derived value, so a
power loss that takes one back, or leaves its file torn, costs a recomputation and nothing else.
`swarmops derive` derives the eager values ahead of any reader, and a value not derived yet is
derived by the first reader that asks for it. `verify --deep` checks the derived values against the
objects.

Explained in: [README, Commands](../README.md#commands) ·
[architecture, Writing](architecture.md#writing).

## External id

An external id is an id of another system, a git commit's oid say, that names a changeset. The
external id index, `meta/external_ids/`, says which changeset each one names, an immutable file per
id, written by `git import` and `git export`. It is a cache outside the journal.

An imported changeset also keeps its commit's id in its `git.oid` metadata.

Explained in: [README, The store on disk](../README.md#the-store-on-disk) ·
[architecture, Git import](architecture.md#git-import) ·
[architecture, Git export](architecture.md#git-export).

## Lossy class

A lossy class is a kind of content that a changeset cannot hold as git has it. `git import` refuses
such content with `E_GIT_IMPORT_LOSSY`, naming the commit, the path and the class, unless
`--lossy CLASS` (repeatable, or `all`) drops or normalizes that class instead.

| Class | Content |
| --- | --- |
| `gitlinks` | A submodule's entry. |
| `paths` | A name that is not UTF-8, or is `.` or `..`. |
| `modes` | A mode git does not write. |
| `tree_order` | A tree out of git's order. |
| `empty_trees` | An empty directory. |
| `nonstandard` | A commit whose bytes its fields cannot give back. A signature date git cannot read is kept as 0 seconds at `+0000`. |

A changeset imported with a loss names the class in its `git.lossy` metadata, and a history imported
with a loss exports as new commits. `--gitlinks drop` is `--lossy gitlinks`.

Explained in: [README, Bring a git repository in](../README.md#bring-a-git-repository-in) ·
[operations, Limits](operations.md#limits).

## FORMAT

`FORMAT` is the file that names a store's layout version: `swarmops-store 1` for every store today.
`init` writes it last, so a store without it was never finished. A binary opens only a store whose
`FORMAT` it knows, and refuses any other as `E_NOT_A_REPO` (exit 6).

A new `FORMAT` comes with the release that needs it, and the changelog says so. SwarmOps is pre-1.0,
and before 1.0 there is no migration tool: a store may have to be created again. The hash contract
does not change with it: a changeset's id is the same under every `FORMAT`.

Explained in: [operations, Upgrades](operations.md#upgrades) ·
[versioning policy, Public surfaces](policies/versioning-and-stability.md#public-surfaces).
