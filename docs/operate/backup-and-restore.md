# Backup and restore

Copy a store that agents are writing, check the copy, and put it back after damage.
Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages).

## Before you start

- A store ([Stores](stores.md)), `flock` and, for a large store, `rsync`.
- To take back what was exported after a backup: the git side, `GIT_SIDE`, of the
  [daily loop](daily-loop.md#without-a-remote-a-bare-clone).

## What a copy may rely on

These come from the store's own guarantees (storage-core spec §2 and §3):

- **Objects never change.** An object is written to `objects/tmp/`, synced, then linked into
  `objects/<2 hex>/<62 hex>`; no one rewrites or removes it afterwards.
- **The journal is the truth.** `meta/journal.log` records every write transaction as one frame,
  and a write is committed once its frame is synced. The index, the ref files and the checkpoint
  are projections of the journal, which the next writer brings up to date.
- **Every journal, index and ref write holds `meta/lock`**, an `flock`. Object writes take no lock,
  but no frame names an object before the object is durable; derived values and external id entries
  take none either, and are caches that no frame names. Readers take no lock.
- **A crash at any point leaves a store that the next writer repairs.**

## Choose how to copy

- **A copy made while you hold `meta/lock` is consistent.** `flock` takes the lock the writers
  take: while it runs, no frame is appended and no ref moves, and the objects that writers add
  meanwhile are ones no frame in the copy names. Writers wait up to 10 s for the lock, then fail
  with the retryable `E_BUSY`: hold it briefly, or pause the agents.
- **An atomic snapshot of the store's filesystem** (an LVM snapshot of its volume, or a btrfs
  snapshot of its subvolume) holds what a crash of every writer at that instant would leave, which
  the next writer repairs.
- **A plain copy of a store in use is not safe.** `cp`, `rsync` or `tar` without the lock can copy
  a ref file that names a changeset newer than the journal it copied, a state no crash produces.

## Back up a store

A backup, under the lock, and its check (`BACKUP` must not exist yet: `cp -a` would copy the store
into it):

```sh
BACKUP=~/backups/acme-$(date +%F)
mkdir -p ~/backups
flock "$SWARMOPS_REPO/meta/lock" cp -a "$SWARMOPS_REPO" "$BACKUP"
swarmops --repo "$BACKUP" verify --deep
```

```json
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":42,"changesets_checked":14,"problems":[],"findings":[]},"meta":{"repo":"…","elapsed_ms":2,"resolved":{},"auth":"trust"}}
```

## Back up a large store

For a large store, copy the objects first without the lock (they never change), then hold the
lock only while `rsync` copies the rest and the objects added since; `objects/tmp/` holds
unfinished writes and is left out (a store creates it again when it writes):

```sh
mkdir -p "$BACKUP"
rsync -a --exclude=/tmp/ "$SWARMOPS_REPO/objects/" "$BACKUP/objects/"
flock "$SWARMOPS_REPO/meta/lock" rsync -a --delete --exclude=/objects/tmp/ "$SWARMOPS_REPO/" "$BACKUP/"
swarmops --repo "$BACKUP" verify --deep
```

## The git mirror and the remotes' files

A store with a git remote holds three more things in its directory
([GitHub as a remote](../guides/github-remote.md)):

- **`remotes.toml`** names each remote, its URL and the branch `main` follows, and
  **`authors.toml`** the address each agent family signs new commits with. Both are small files
  that SwarmOps or a person writes whole; back them up with the store, as the copies above do.
  Neither holds a credential.
- **`git/mirror.git`** is a cache: the remotes' commits as git fetched them, and the exports a push
  sent. It needs no backup. A copy may leave it out (`--exclude=/git/mirror.git/` in both `rsync`
  commands): a store without it, or with one that a fetch was writing while it was copied, makes it
  again at its next `fetch` (or `sync`, which fetches), which moves a damaged one aside as
  `git/mirror.git.broken-<time>` and never deletes it. `meta/lock` does not stop the mirror's
  writers; `git/lock` does, for the time a `fetch`, `push`, `sync` or `remote` command runs.

## What writers see while you hold the lock

A writer that meets the held lock waits, then gives up:

```json
{"ok":false,"op":"commit","api":1,"error":{"code":"E_BUSY","message":"the metadata store is busy: another writer holds meta/lock","retryable":true,"exit":4,"details":{},"hint":"retry: another writer held the store's lock for the whole wait"},"meta":{"repo":"…","elapsed_ms":10017,…}}
```

## Restore a store

To restore, stop every writer first (the agents' sessions and their MCP servers, and any script),
put the damaged store aside, copy the backup into place and check it. Then take back from git
what was exported after the backup, from the review branch, which holds every export (the trunk
holds those that were promoted):

```sh
mv "$SWARMOPS_REPO" "$SWARMOPS_REPO.damaged"
cp -a "$BACKUP" "$SWARMOPS_REPO"
swarmops verify --deep
swarmops git import "$GIT_SIDE" --ref review --into main
swarmops git export "$GIT_SIDE" --ref main --branch review
```

```text
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":42,"changesets_checked":14,"problems":[],"findings":[]},…}
{"ok":true,"op":"git_import","api":1,"result":{"imported":1,"skipped":1,"head_changeset":"ebbd2134…","git_head_sha":"327bbc8898086c815a71e9ca1ab69bec147e5e36","target_ref":"main","ref_moved":true,"old_head":"c250bd46…","lossy":[],"lossy_total":0},…}
{"ok":true,"op":"git_export","api":1,"result":{"exported":0,"objects_written":0,…,"branch_moved":false,"incremental":true,"rewritten":0},…}
```

The land exported after the backup comes back as the commit it was exported as, and the export
finds nothing to write. It is an imported changeset now: `log` shows its agent as
`unknown/unknown`, and its signatures keep the git author, the agent that made it. What is lost is
what was written after the backup and never exported: changesets no export reached (an agent's
unlanded commits, lands made since the last export), ref moves, and the operation log's entries.

## Without a backup

The git side is itself a copy of everything exported. Without any backup, a new store made from it
(`swarmops init`, then `swarmops git import` of each branch) exports to the same commits.
