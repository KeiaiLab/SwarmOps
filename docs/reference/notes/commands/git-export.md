### What a run does

`git export` is a client-side command: it reads the store that `--repo` (else `SWARMOPS_REPO`)
names and writes a git repository on this host. No server ever sees the request, and there is no
MCP tool for it. A run goes in this order, each step done before anything of the next is written:

1. **Check the names.** `--ref` must be a ref name of the store (`main` or `release/<name>`),
   checked before the repository is opened. The repository is opened as the import opens one
   (isolated from global configuration and `GIT_*` variables, the same repositories refused), and
   `--branch` must be a branch name git takes.
2. **Refuse a checked-out branch.** In a work tree, the branch its `HEAD` names is refused with
   `E_INVALID_ARG` (exit 2, `details.reason` `checked_out`), with or without `--force`, before the
   store is read.
3. **Read the head.** The head of `--ref` is read in a committed snapshot, taken under the store's
   writer lock after the journal's tail check, so that nothing the export sends off the host can
   be taken back by a power loss or a failed sync.
4. **Derive the export ids.** The commit id each changeset exports to is derived from the store's
   own objects, for the head and the whole history below it.
5. **Compare with the branch.** A branch at the head's export is up to date, and nothing is
   written. A branch that exists must name the export of the head or of an ancestor of it, the
   base; anything else is `E_GIT_EXPORT_DIVERGED`, unless `--force`.
6. **Write.** The commits the branch does not reach, parents first; for each, the trees and blobs
   its first parent does not hold at the same path, then the commit. An object the repository
   holds in a pack is trusted; a loose one is read back, and written again if it is damaged.
7. **Make it durable.** Every object written or read back is fsynced.
8. **Record.** The ids of the commits made in the store go into the store's external id index,
   each naming its changeset.
9. **Move the branch.** The branch moves to the head's export by compare-and-swap, through git's
   own lock file `refs/heads/<branch>.lock`, or is created.

### What the store records

Nothing in its history. An export writes no journal frame, so it needs no identity, does not show
in `op log`, and the page's metadata says `mutates: false`. Its writes to the store are two
caches, the derived values and the external id entries. `meta.resolved` names the head the source
ref held, under the ref's name, on success and on a divergence; `meta.recovered` says what the
snapshot's tail check repaired, on success only.

### Choose the source: `--ref`

The store ref whose history goes out, `main` by default, or `release/<name>`. A ref the store does
not have fails with `E_REV_NOT_FOUND` (exit 6), whose hint lists the refs; a name that is no ref
name, such as `feature/x`, with `E_REF_NAME_INVALID` (exit 1).

### Choose the branch: `--branch`

The git branch written, `refs/heads/<BRANCH>`, `main` by default; name it after the remote's trunk
(`--branch stable`), or after a branch that waits for review (`--branch review`). A branch that
does not exist is created, and `incremental` is `false`. A name git does not take, such as `a..b`,
fails with `E_REF_NAME_INVALID`, whose hint is `git check-ref-format --branch <name>`.

The repository must exist already: a path with no repository is `E_GIT_NOT_A_REPO` (exit 6), whose
hint is `git init --bare <path>`. A bare repository has no checked-out branch, so it is the one to
export into.

### Replace a diverged branch: `--force`

`--force` moves a branch that diverged all the same, replacing what git holds there, and adds a
warning that says so (`the branch <name> had diverged (<reason>), and the export moved it all the
same`). The move is still a compare-and-swap against the commit the export read, so a branch
another writer moves meanwhile still stops it (`moved_during_export`). When the divergence found
the branch at the export of a changeset of the history, the export goes on from there, writes
only what the branch lacks and reports `incremental: true`.

`--force` never moves a checked-out branch, and does nothing for an `E_GIT_WRITE`.

### What git gets

- **An imported changeset** exports to the commit it came from, byte for byte, as long as nothing
  below it was imported with a loss.
- **A changeset made in the store** exports to a commit with its own author and committer, named
  `<family>/<instance>` with the address `<instance>@<family>.swarmops.local`, and a trailer block
  after its message: `SwarmOps-Changeset:` with the changeset's id, then `SwarmOps-Land-Key:` and
  `SwarmOps-Squashed-From:` for a landed change. The same store gives the same commit every time.
- **A history imported with a loss** exports as new commits, from the first lossy commit up:
  `rewritten` counts them, a warning says so, and the signature headers they keep no longer
  verify.

[What git gets](../../../guides/git-round-trip.md#what-git-gets) shows such a commit in
`git log`.

### Run it again

The export is incremental: run again, it writes only what the branch's last export does not reach.
With nothing new it writes nothing: `exported` 0, `objects_written` 0, `branch_moved` `false`.

`exported` counts the commits written that the repository did not hold, so exporting a history the
repository holds already into a new branch writes no object and only creates the branch.

### When the branch diverged

`E_GIT_EXPORT_DIVERGED` (exit 3) means the branch names something other than the export of the
head or of an ancestor of it. Nothing is written. `details` give the `reason`, the `branch`, the
commit it names (`git_head`), the source's head (`head_changeset`) and, for `branch_ahead`,
`both_moved` and `source_moved_back`, the changeset whose export the branch holds
(`exported_changeset`). The hint is the next step:

| `reason` | What happened | Hint |
| --- | --- | --- |
| `branch_ahead` | Commits were made in git on top of the export of the head. | `swarmops git import <path> --ref refs/heads/<branch> --into <ref>`, then export again. |
| `both_moved` | Commits were made in git on top of an older export, and the source moved too. | `git -C <path> log refs/heads/<branch>` shows the git side; `--force` replaces it. |
| `branch_moved` | The branch names a commit that is no export of this history. | `git -C <path> log refs/heads/<branch>`; `--force` replaces it with the export. |
| `source_moved_back` | The branch holds the export of a changeset that is no ancestor of the head, as after an `op undo` or a `ref set` that took a `release/<name>` source back. | `swarmops op log` names what moved the source; `--force` moves the branch back. |
| `lossy_history` | The branch holds the original commits of a history imported with a loss. | Export to another branch, or `--force` replaces the originals. |
| `moved_during_export` | Another writer moved the branch while the export wrote; `git_head` is null when it deleted it. | Run the same export again. What it wrote stays. |

### Patterns

**Export to an empty repository.** Make a bare one, export, and check it with git:

```sh
git init --bare ~/src/mirror.git
swarmops git export ~/src/mirror.git
git -C ~/src/mirror.git fsck --strict --no-dangling
```

**Send landed work to review.** Export into a branch of its own, push it, and move the trunk with
git once the review passes ([the daily loop](../../../operate/daily-loop.md#4-send-it-to-git-for-review)):

```sh
swarmops git export ~/src/acme.git --ref main --branch review
git -C ~/src/acme.git push origin review
```

**Continue a branch that people also push to.** Fetch and import before agents land more, so that
the next export continues the branch; when it stops with `branch_ahead`, run the import its hint
names, then export again.

**Take a release back, in git too.** After `swarmops op undo` takes a `release/<name>` source back,
the export stops with `source_moved_back`; `--force` moves the branch back to the export of the new
head, and the push after it must be forced too. An undo never takes `main` back
(`E_REF_PROTECTED`): land a change that reverts the landed one, and the export continues the
branch.

### Concurrency and interruptions

- **Agents.** The committed snapshot is taken under the store's writer lock, so an export waits
  behind committing agents as a writer does, and past the lock's timeout fails with `E_BUSY`
  (exit 4, retryable). Agents commit while it writes git: the export sends the head it read.
- **Git.** The branch moves through `refs/heads/<branch>.lock`. A lock file that is there already
  is `E_GIT_WRITE` with `details.reason` `ref_locked`: another git process holds it, or an export
  that died left it. Remove it once no git process runs ([known limitations](../../../known-limitations.md#git-import-and-export)).
- **A cut-short export** leaves loose objects that no branch reaches. The same export run again
  reads them back, writes again any that is damaged, and writes only what is still missing.

### Failure cases

| Code | Exit | When |
| --- | --- | --- |
| [`E_REF_NAME_INVALID`](../../errors.md#e_ref_name_invalid) | 1 | `--ref` is not `main` or `release/<name>`, or `--branch` is no branch name git takes. |
| [`E_GIT_NOT_A_REPO`](../../errors.md#e_git_not_a_repo) | 6 | No git repository at the path. |
| [`E_GIT_UNSUPPORTED`](../../errors.md#e_git_unsupported) | 1 | A repository the export does not write as it is: SHA-256, reftable refs, a shallow clone, a special or too long file in the git directory. |
| [`E_INVALID_ARG`](../../errors.md#e_invalid_arg) | 2 | The branch is checked out in the repository's work tree (`details.reason` `checked_out`). |
| [`E_REV_NOT_FOUND`](../../errors.md#e_rev_not_found) | 6 | The store has no `--ref`. |
| [`E_GIT_EXPORT_DIVERGED`](../../errors.md#e_git_export_diverged) | 3 | The branch diverged; nothing written. |
| [`E_GIT_WRITE`](../../errors.md#e_git_write) | 1 | `ref_locked`, `ref_name_conflict` (a ref in the branch's way), or `io` (a write the filesystem refused). Objects written stay; the branch did not move. |
| [`E_GIT_READ`](../../errors.md#e_git_read) | 1 | An object of the repository is missing or damaged, or a read failed. |
| [`E_BUSY`](../../errors.md#e_busy) | 4 | The writer lock stayed held past its timeout. |
| [`E_STORE_CORRUPT`](../../errors.md#e_store_corrupt) | 1 | An object of the history below the head is missing or damaged; `swarmops verify` lists it. |

### Cost

Each object an export writes is a loose file, fsynced on its own, so the first export of a long
history writes one file and one fsync per object until `git gc` packs them. Every export, an
incremental one included, derives the export id of each changeset below the head again from the
store's objects. [Known limitations](../../../known-limitations.md#git-import-and-export) gives both
costs; the [performance ledger](../../../perf.md) has no measurement of the export yet.

### Go further

- [Keep git: import, land and export](../../../guides/git-round-trip.md#export-back-to-git) runs
  the round trip, and [When an export stops](../../../guides/git-round-trip.md#when-an-export-stops)
  walks through `branch_ahead`.
- [One writer per trunk](../../../operate/one-writer-per-trunk.md#export-refusals-and-their-reasons)
  keeps the review branch and the trunk apart.
- [The git side](../../../operate/monitoring.md#the-git-side) says what to watch.
- [Git export](../../../architecture.md#git-export) in the architecture overview walks through each
  step.
