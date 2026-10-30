### What a run does

`git import` is a client-side command: it reads a git repository on this host and writes the
store that `--repo` (else `SWARMOPS_REPO`) names. No server ever sees the request, and there is no
MCP tool for it. A run goes in this order:

1. **Check the target.** `--into` must be a ref name of the store (`main` or `release/<name>`),
   checked before the repository is opened: `--into feature/x` is `E_REF_NAME_INVALID`, whatever
   the path holds.
2. **Open the repository.** The path is a work tree that holds `.git`, or a bare repository, taken
   relative to the working directory. It is opened isolated: no global or system git
   configuration and no `GIT_*` variable apply, and replacement objects (`refs/replace/`) and
   grafts never do. A SHA-256 repository, reftable refs, a shallow clone, a FIFO, a socket or a
   device where a file of the git directory is read, and such a file read whole that is longer
   than 16 MiB are refused here, before any object is read.
3. **Resolve `--ref` and read the target.** The tip is the commit `--ref` names. A target that
   exists has a base: the git commit its head changeset holds in its `git.oid` metadata, or, for
   a changeset made in the store, the commit it exports to.
4. **Walk.** From the tip over every parent, first parents first, the walk stops at the base and
   at every commit the store already holds an import of. What it stopped at is `skipped`.
5. **Check the ancestry.** The base must be the tip (then nothing is written) or an ancestor of
   it. Anything else is `E_GIT_IMPORT_DIVERGED`, and nothing is written.
6. **Convert and record.** The new commits become changesets, parents first, up to 8,192 to a
   journal frame. Their trees and blobs are stored first, and each frame's commits get their
   external id entries once the frame is durable.
7. **Move the target.** The target moves from the head it had to the tip's changeset by
   compare-and-swap, recorded for your identity. A target that does not exist is created.

### Choose the commit: `--ref`

`--ref` names one commit, and the import takes it with every commit it reaches:

- `HEAD`, the default: the repository's own `HEAD`, which in a bare clone names the remote's
  default branch.
- A ref's name, short or full, looked up in git's order: as given, then under `refs/`,
  `refs/tags/`, `refs/heads/`, `refs/remotes/`, and as `refs/remotes/<name>/HEAD`. An annotated
  tag is peeled to its commit.
- A commit id of 40 hex digits, either case, which wins over a ref of the same name.

There is no revision syntax: `--ref stable~1` names no ref and fails with `E_GIT_REF_NOT_FOUND`
(exit 6), whose hint lists the repository's refs (`git -C <path> for-each-ref`). A name that
peels to a tree or a blob fails the same way, with `details.kind` saying what it named.

### Choose the target: `--into`

`--into` is the store ref the import moves, `main` by default, or `release/<name>`; a store has no
other refs. It decides where the import goes on from:

| The target | What the import does |
| --- | --- |
| Does not exist | Imports everything `--ref` reaches that the store does not hold, then creates the target. |
| Holds an imported changeset | Goes on from the commit that changeset came from (`git.oid`). |
| Holds a changeset made in the store | Goes on from the commit that changeset exports to, so that commits made in git on top of an export come back on the changeset they were made on. |
| Holds `--ref`'s commit already | Writes nothing: imports 0, skips 1, and `ref_moved` is `false`. |

A head that `--ref` does not reach (a rewritten git history, or a change landed in the store and
not exported yet) fails with `E_GIT_IMPORT_DIVERGED` (exit 3). The details name the target, its
head, the git commit it stands for (`base`) and the tip (`git_head`). The hint is the target's
log, or `swarmops verify --deep` when the walk passed over an external id entry whose changeset
stands for another commit (`details.external_id`). Nothing is merged.

A ref of its own keeps a git branch apart from `main`: `--into release/<name>` imports it there,
and `land` can then take it onto `main`.

### Allow loss: `--lossy` and `--gitlinks`

By default the import keeps every commit exactly, and refuses one it cannot keep with
`E_GIT_IMPORT_LOSSY` (exit 1): `details` names the `commit`, the `class` and the `path` (also as
`path_base64` when it is not UTF-8; empty for `nonstandard`), and the hint names the `--lossy`
class that would allow it. The import stops before the frame that would hold that commit, and the
target does not move.

`--lossy <class>` allows one class; it repeats, and `all` allows every class. `--gitlinks` decides
the `gitlinks` class alone, whatever `--lossy` says:

| Line | Classes allowed |
| --- | --- |
| `--lossy gitlinks` | `gitlinks` |
| `--gitlinks drop` | `gitlinks` |
| `--lossy all --gitlinks refuse` | every class but `gitlinks` |
| `--lossy gitlinks --gitlinks refuse` | none |
| `--lossy modes --lossy tree_order` | `modes`, `tree_order` |

A changeset taken with a loss names its classes in its `git.lossy` metadata. The result lists the
first 100 such commits in `lossy`, each with its changeset and classes, in the order imported, and
`lossy_total` counts them all. [What import refuses](../../../guides/git-round-trip.md#what-import-refuses)
says what each class drops or normalizes.

A loss shows again at the export: a commit taken with a loss renders to other bytes than the commit
it came from, and so does every commit above it. [`git export`](../../commands/git-export.md)
writes them as new commits (`rewritten`), and refuses to put them on a branch that holds the
originals (`lossy_history`).

A commit the store holds already is reused as it is, so a later import without `--lossy` that
reaches a commit an earlier import took with a loss skips it rather than refusing it, and reports
`lossy_total` 0 ([known limitations](../../../known-limitations.md#git-import-and-export)).

### What the store records

- **One changeset per commit.** Its author, committer and their times are the commit's, its
  `git.oid` metadata the commit's id, and a merge keeps all its parents in their order. The same
  git commit is the same changeset in every store.
- **No agent.** An imported changeset records no agent: `log`, `show` and `op log` give its agent
  as `unknown/unknown`, and `show`'s `signatures` give the git author and committer.
- **Operations in `op log`.** Each journal frame of new changesets is an operation of kind
  `commit`. The target's move is an operation of its own, of kind `ref_move`, whose actor is your
  identity. An import into a ref of its own that finds every commit known writes only the
  `ref_move`.
- **External ids.** Each imported commit gets an external id entry naming its changeset. That is
  how a later run knows where to stop, and how an export finds the commit again.
- **`meta.recovered`.** What the import's writes repaired of what earlier writers left, added up,
  on success only.

### Run it again

The import is incremental. Run again, it walks only until it meets a commit the store holds, and
takes only the commits above it. With nothing new it writes nothing: `imported` 0, `skipped` 1,
`ref_moved` `false`, and `old_head` the head it found.

`skipped` counts where the walk stopped, not the history the store holds: the base, and each
commit an earlier import made that the walk met. Importing the same commit into a second ref
writes no changeset, imports 0, skips 1, and only creates that ref.

### Patterns

**Bring a repository in, once.** Import from a bare clone, which the export can later write as
well:

```sh
git clone --bare <remote-url> ~/src/acme.git
swarmops git import ~/src/acme.git --ref stable --into main
```

**Take in what git gained.** Fetch, then import the same branch into the same ref. Only the new
commits come in, and `main` moves to the last of them:

```sh
git -C ~/src/acme.git fetch origin stable:stable
swarmops git import ~/src/acme.git --ref stable --into main
```

**Bring commits made in git on top of an export.** When an export stops with `branch_ahead`, its
hint is the import that brings those commits in, by the branch's full name:

```sh
swarmops git import ~/src/acme.git --ref refs/heads/stable --into main
```

**Land a git branch.** Import it into a ref of its own, land that ref onto `main`, and delete the
ref with the id it holds; [One writer per trunk](../../../operate/one-writer-per-trunk.md#change-the-trunk-without-breaking-the-rule)
runs it end to end.

**Take back an import's move.** `swarmops op log` shows the target's move as a `ref_move` operation,
and `swarmops op undo <id>` of its `id` moves the target back to what it named before, or deletes it
if the import created it. The changesets stay in the store, so the same import run again writes no
changeset and moves the target again.

### Interruptions and concurrency

The import writes frame by frame, each frame one write transaction under the store's writer lock,
so agents commit and land between its frames. The target moves last:

- **The target moved during the import** (an agent landed on it): `E_CAS_MISMATCH` (exit 4,
  retryable), with every commit recorded. The same import run again moves the target if what it
  names leads to the tip, and fails with `E_GIT_IMPORT_DIVERGED` otherwise.
- **The writer lock stayed held.** A frame that waits out the lock is tried again three times,
  then the import stops with `E_BUSY` (exit 4).
- **A store failure** (`E_IO_TRANSIENT`, `E_IO`, `E_STORE_CORRUPT`, …) stops the import after the
  last frame that committed, and it never writes again in that run. `details.imported` and
  `details.last_commit` (null for none) say how far it got, and the hint is to run the same import
  again, which continues after them, unless the store is damaged (`swarmops verify`). A frame
  that committed but failed after its commit point is `E_IO` with `details.committed` `true`.

### Failure cases

| Code | Exit | When |
| --- | --- | --- |
| [`E_IDENTITY_REQUIRED`](../../errors.md#e_identity_required) | 5 | No `--as` and no `SWARMOPS_AGENT`: checked before the store is opened. |
| [`E_REF_NAME_INVALID`](../../errors.md#e_ref_name_invalid) | 1 | `--into` is not `main` or `release/<name>`. |
| [`E_GIT_NOT_A_REPO`](../../errors.md#e_git_not_a_repo) | 6 | No git repository at the path. |
| [`E_GIT_UNSUPPORTED`](../../errors.md#e_git_unsupported) | 1 | A repository or commit the import does not read; `details.reason` says which. A `shallow` clone's hint deepens it (`git -C <path> fetch --unshallow`). |
| [`E_GIT_REF_NOT_FOUND`](../../errors.md#e_git_ref_not_found) | 6 | `--ref` names nothing, or no commit. |
| [`E_GIT_READ`](../../errors.md#e_git_read) | 1 | An object is missing (a partial clone lacks blobs), damaged or unparsable, with the hint `git -C <path> fsck`; or a read failed (`io`). |
| [`E_TOO_LARGE`](../../errors.md#e_too_large) | 1 | An object longer than a store object holds, refused by its header before it is read. |
| [`E_GIT_IMPORT_LOSSY`](../../errors.md#e_git_import_lossy) | 1 | Content of a class the line does not allow. |
| [`E_GIT_IMPORT_DIVERGED`](../../errors.md#e_git_import_diverged) | 3 | The target's head is not the import of the tip or of an ancestor of it. Nothing is written. |
| [`E_CAS_MISMATCH`](../../errors.md#e_cas_mismatch) | 4 | The target moved while the import wrote. Every commit is recorded. |
| [`E_BUSY`](../../errors.md#e_busy) | 4 | The store's writer lock stayed held through three retries. |

### Cost

An import reads and converts each new tree and blob once, and seeds its conversion with what the
target's head holds, so an incremental run reads mostly what changed. It holds the new files of one
commit in memory together, and onto a target whose head was made in the store it walks the whole
history below that head; [known limitations](../../../known-limitations.md#git-import-and-export)
gives both costs. The [performance ledger](../../../perf.md) has no measurement of the import yet.

### Go further

- [Keep git: import, land and export](../../../guides/git-round-trip.md) runs the round trip
  command by command.
- [The daily loop](../../../operate/daily-loop.md#once-bring-the-repository-in) brings a
  repository in and [takes in what git gained](../../../operate/daily-loop.md#1-take-in-what-git-gained).
- [One writer per trunk](../../../operate/one-writer-per-trunk.md#tell-a-divergence-from-a-lagging-trunk)
  tells a real divergence from a trunk that waits for its review.
- [Git import](../../../architecture.md#git-import) in the architecture overview walks through each
  step.
