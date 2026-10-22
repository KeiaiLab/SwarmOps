---
title: "How SwarmOps works"
description: "The crates, the store on disk, the write and read paths, landing, durability and concurrency of SwarmOps, in one overview."
---

# How SwarmOps works

An overview of how SwarmOps is built, as of v0.1.0. The
[architecture document](/docs/architecture/) gives every detail; this page is the short form.

## The layers

Every operation is a library function. The binary parses a command line, calls one operation and
prints its envelope; `swarmops mcp` calls the same operations for MCP clients. An arrow points from
a crate to one it depends on (shortcuts left out).

```text
Fig. 1: the crates

            agents and people
                    │  a command, or an MCP tool call
                    ▼
  swarmops-cli ──────────────────► swarmops-api
  `swarmops` and `so`;             the contract: request and response
  `swarmops mcp` for MCP           types, E_* codes, the envelope, StoreOps
        │                                 ▲
        ▼                                 │
  swarmops-ops ───────────────────────────┘
  LocalOps: StoreOps over a local store
        │                 │
        │                 ▼
        │           swarmops-git
        │           git import and export, through gix
        ▼                 │
  swarmops-repo ◄─────────┘
  the repository engine
        │                 │
        ▼                 ▼
  swarmops-store     swarmops-merge
  object files and   tree and line diffs, the
  the metadata store three-way merge; no I/O
        │                 │
        ▼                 ▼
          swarmops-core
          ids, paths, the canonical codec; no I/O
```

| Crate | What it holds |
| --- | --- |
| `swarmops-core` | Object ids, repository paths and the canonical object codec. No I/O; its byte layouts are the hash contract. |
| `swarmops-store` | The object files, the store's layout, and the metadata store: the journal, the writer lock, the changeset index and the ref files; the cache of derived values beside it. |
| `swarmops-merge` | The tree diff, the line diff (histogram) and the three-way merge. No I/O, no store. |
| `swarmops-repo` | The repository engine: commits from change lists, reads at a revision without a checkout, first-parent history, ref moves by compare-and-swap, `verify`, derived values. |
| `swarmops-git` | git's side of the import and the export: git's bytes parsed and rendered back byte for byte, read and written through gix. |
| `swarmops-api` | The contract: serde and JSON Schema types for every request and response, the error codes, the envelope, `StoreOps`. |
| `swarmops-ops` | `LocalOps`, `StoreOps` over a local store: it resolves revisions and identities, calls the repository, and maps every failure to an `E_*` code. |
| `swarmops-cli` | The `swarmops` binary and `so`, the same program by a shorter name. |

Store-facing types carry no filesystem paths: every operation takes and returns serde types, so
`StoreOps` is the seam a remote client and a server will share (M11's sync over ssh). A change's
`{"file": "<path>"}` content is read on the client.

## The data model

Three kinds of immutable, content-addressed objects:

- A **blob** is a file's content, raw bytes.
- A **tree** is one directory: entries sorted by name, each a kind (file, exec, symlink or dir), a
  name and an id. There are no empty directories.
- A **changeset** is one commit: its parents, the id of the whole resulting tree (`root`), what
  changed against its first parent (`delta`), git-style author and committer, the message, and
  metadata SwarmOps owns, such as the committing agent.

A changeset carries both the full root and the delta: the root makes a read a walk of the path's
depth with no checkout, and the delta keeps a commit's cost proportional to the size of the change.

An id is BLAKE3-256 over a header that names the object's kind and layout version, the payload's
length and the payload: 64 hex digits, and on the command line any unique prefix of 12 or more.
The codec refuses every non-canonical encoding, so each object has exactly one valid byte string.
These layouts never change in place: a new layout gets a new kind/version header.

## The store on disk

A store is one directory:

```text
<store>/
  FORMAT                   the layout version, written last by init
  REPO                     the repository's id and name
  objects/<2 hex>/<62 hex> one self-verifying file per blob, tree and changeset
  objects/tmp/             where new object files are staged
  meta/lock                the lock that serializes metadata writers
  meta/journal.log         append-only, CRC-framed journal: the source of truth
  meta/journal.off         each frame's offset
  meta/journal.ckpt        how far the journal is materialized and synced
  meta/idx/, parents.log, sorted/   the changeset index, built from the journal
  meta/refs/               one file per ref: main, release/<name>
  meta/derived/            derived values: a cache, never synced
  meta/external_ids/       the git commit ids of imported and exported changesets
```

Objects are compressed one at a time with zstd when that makes them smaller. The index and the
ref files are projections of the journal; every object is durable before any metadata names it.

## Writing: a commit

```text
Fig. 2: a commit

 1. write new blobs, trees, changeset ──► objects/   (no lock; temp file, sync, link)
 2. take meta/lock (flock) ───────────── E_BUSY after 10 s, retryable
 3. repair what a crash left ─────────── reported in meta.recovered
 4. append one frame, fdatasync ──────── THE COMMIT POINT
 5. list the frame in journal.off, sync
 6. materialize: index records, ref files (temp file, fsync, rename)
 7. every 32 frames: sync the index, move the checkpoint
 8. release the lock
```

A failure before the commit point leaves nothing committed: the writer cuts the frame back and
returns the retryable `E_IO_TRANSIENT` with `details.committed: false`. A failure after it leaves
the transaction committed and durable, reported as a warning, and the next write repairs the rest.

A ref move is a compare-and-swap under the same lock: on a mismatch nothing is recorded and the
caller gets `E_CAS_MISMATCH` with the ref's target in `details.actual`. `commit --advance` puts the
changeset and its ref's move in one frame, recorded together or not at all.

## Landing

`land` publishes a change made on an older head. It merges the change three-way onto the target's
head, without a checkout, and inserts the result, one squashed changeset whose parent is that head,
with the target's compare-and-swap in one transaction.

```text
Fig. 3: two agents land on main

  main:  A ──────► B (agent 2 landed) ──────► C (agent 3 landed)
          \                                  ▲
           └─ agent 3's change, made on A ───┘  merged three-way onto B
```

A land that loses its race merges again onto what the target gained and tries again, up to
`--max-attempts`. A conflict is `E_CONFLICT` with a report of each conflicting file, and nothing is
recorded; `land --resolve` resolves it path by path. A keyed land run again finds its changeset on
the target and replays it.

## Reading

Readers take no lock. A snapshot reads the checkpoint, the index, the frames past the checkpoint
and every ref file, then the frames again; if they changed, it reads again, at most 32 rounds, and
then gives up with the retryable `E_BUSY`. Files and directories at a revision are read from the
trees, starting at the changeset's root, and every object read checks the object's header and hash.
Every revision one request names is resolved in the same snapshot.

## Durability

Full fsync is the default and the only mode. An object is synced and its directory fsynced before
any journal frame names it; a transaction commits when its frame's `fdatasync` returns; ref files
and the checkpoint are replaced by temp file, fsync, rename and directory fsync. A failed sync is
never trusted to a later one.

The price, measured on the maintainers' host and recorded in [perf](/docs/perf/): about 4 ms per
fsync, and about 21 ms for an uncontended durable commit through the binary.

What tests hold the code to it, through the real binary:

- crash sweeps that abort the process at every filesystem call, after which the store must open
  and verify clean;
- injected I/O errors (`EIO`, `ENOSPC`), once or for every write from then on, each outcome honest;
- write-ordering checks on every filesystem call;
- a model of fsync failures on Linux;
- power-loss images: what a disk may hold after the power fails at each step of a recorded run,
  each of which the binary must recover;
- property tests and fuzz targets for the codec and journal recovery;
- multi-process tests: 8 processes committing at once, 16 racing to move one ref, each ending with
  `verify --deep`.

## Concurrency

- **Objects** need no lock: immutable, named by their hash, linked into place without replacing
  anything.
- **Metadata writers** take `meta/lock`, an `flock`. The kernel releases it when a process dies,
  so a crash leaves no stale lock. A waiting writer gives up after 10 s with the retryable
  `E_BUSY`. The queue is not strictly first-in, first-out, but the wait is bounded: with 32
  processes making 1,000 commits each through the binary, none got `E_BUSY` and the slowest commit
  took 918 ms.
- **Readers** take no lock.
- **Commits** only append and do not fail because of another agent; **refs** move only by
  compare-and-swap, and when several agents expect the same target, exactly one wins each round.

## The contract

Every operation is a function from a request type to a response type or an error, and every type
is serde and JSON Schema. Requests and responses ignore fields they do not know, so versions can
differ. Every run prints exactly one envelope on stdout:

```text
{"ok": …, "op": …, "api": 1, "result": … | "error": {code, message, retryable, exit, details, hint}, "meta": {repo, elapsed_ms, resolved, auth, recovered}}
```

A failure's `code` is one of 37 stable `E_*` codes, from one table that also fixes each code's exit
code and whether retrying can help. An I/O failure names the file operation and the store file,
never a host path but in two rare `E_IO`s. Responses are bounded for an agent's context: `cat` at
most 1 MiB by default, `ls` and `log` in pages with a cursor, `show` at most 400 changes by
default.

## Git import and export

`git import` reads a git repository on this host and converts every commit the store does not
hold, parents first, many to a journal frame; then it moves the target ref by compare-and-swap.
`git export` reads the store in a committed snapshot, taken under the writer lock so that nothing it
sends off the host can be taken back, writes the missing commits as loose objects, fsyncs them,
and moves the branch by git's own lock protocol, by compare-and-swap. A commit imported from git
renders back to its own id; one made in the store gets a `SwarmOps-Changeset:` trailer.

## Read on

[Architecture](/docs/architecture/) · [Operations guide](/docs/operations/) ·
[Known limitations](/docs/known-limitations/) · [Features](/product/features/) ·
[SwarmOps and git](/product/vs-git/)
