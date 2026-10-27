# Stores

Choose where a store lives, protect it, create it, and give every writer an identity.
Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages).

## Before you start

- `swarmops` on the host ([Install and upgrade](install-and-upgrade.md)).
- A directory on a native Linux filesystem of the host, with room to grow (below).

## Where a store may live

- **One host.** Every agent, every MCP server and every command that uses a store runs on the
  host that holds it: SwarmOps has no network protocol of its own yet (remote sync is M11).
  Between hosts, git carries the history: through a git remote that each store fetches from and
  syncs with ([GitHub as a remote](../guides/github-remote.md)), or one writer at a time through a
  bare clone ([One writer per trunk](one-writer-per-trunk.md)).
- **A native Linux filesystem for the store**: ext4 with its journal (what SwarmOps is tested and
  measured on), XFS or btrfs. A store under `/mnt/` (where WSL mounts Windows drives) is refused
  with `E_UNSUPPORTED_FS`; NFS, SMB and FUSE mounts are not detected yet, and not supported.
- **The host's local disk, with room to grow.** A store keeps one file per object
  (zstd-compressed when that is smaller), packs nothing and removes nothing yet.

How much room: a new store takes about 1 MB (its 256 object directories); SwarmOps' own history,
422 commits, took 51 MB in 8,085 files in the pilot's dry run of 2026-10-20 ([pilot.md](../pilot.md)),
and 57 MB in 8,854 files at 470 commits, measured again later that day. The first export of a
history then writes one small file of derived git ids per object it exports, under
`meta/derived/`, and later exports one per object none exported before: at 470 commits the first
export wrote 8,372 of them, and the store took 92 MB in 17,232 files. Count inodes as well as
bytes ([Monitoring](monitoring.md#disk)); [Making room](making-room.md) says what may go.

## Set the permissions

**One Unix user.** The store's object files, ref files and checkpoint are created readable by
their owner only (mode `0600`), so every agent, MCP server and command that uses a store runs as
the user who owns it. The store directory's filesystem permissions are its only access control
([SECURITY.md](../../SECURITY.md)). Those `0600` files are the objects, the refs, the checkpoint,
the external ids, `FORMAT` and `REPO`. The rest follow the umask: the store's directories, its
journal (`meta/journal.log` and `meta/journal.off`), `meta/parents.log`, `meta/lock`, the index
columns, the sorted views and the derived values (`0775` and `0664` under a umask of `0002`:
whoever can reach the store's directory can then list it and read its journal, and the store's
group can write to it).

So keep a store meant for one user in a directory of mode `0700`, made before the first
`swarmops init` under it (on a directory that exists, it sets the mode):

```sh
install -d -m 0700 ~/.local/state/swarmops/repos
```

## Create a store

A store is one directory. Every command finds the store by `--repo` or `SWARMOPS_REPO`; there is
no default location, and `~/.local/state/swarmops/repos/<name>` is only the convention used here.

```sh
export SWARMOPS_REPO=~/.local/state/swarmops/repos/acme
swarmops init "$SWARMOPS_REPO" --name acme
```

```json
{"ok":true,"op":"repo_init","api":1,"result":{"repo_id":"101d13d0eebd438776f99316f817dad6","name":"acme","store_version":2,"changesets":0,"refs":0},"meta":{"repo":"…","elapsed_ms":52,"resolved":{},"auth":"trust"}}
```

The directory holds `FORMAT` (`swarmops-store 2`, the layout's version; `init --format 1` makes one that 0.3.0 and earlier open), `REPO` (the repository's
id and name), `objects/` and `meta/`; [the README](../../README.md#the-store-on-disk) and
[architecture.md](../architecture.md) describe them. Nothing runs in the background: there is no
daemon, and each command or MCP call opens the store, does its work and closes it.

To check the new store, run `swarmops verify` ([Verify and repair](verify-and-repair.md)).

## Identities

Reads need no identity. A commit, a land, a ref move, an undo and a git import need one:
`--as <family>/<instance>`, else `SWARMOPS_AGENT` (a family of `[a-z0-9-]`, an instance of
`[A-Za-z0-9._-]` that does not start with `.`), with `SWARMOPS_RUN_ID` naming the agent's run if
it has one. The store records it on the changeset and on each ref move, and `git export` writes it
as the commit's author or committer, `<family>/<instance> <instance@family.swarmops.local>`. Give
every agent process its own instance, and yourself one of your own, such as `operator/ada`.
`git export` needs none.

To check the identity a command runs with:

```sh
swarmops --as operator/ada whoami
```

```json
{"ok":true,"op":"whoami","api":1,"result":{"agent":"operator/ada","auth":"trust"},"meta":{"elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

Next: [give agents the store](agents.md).
