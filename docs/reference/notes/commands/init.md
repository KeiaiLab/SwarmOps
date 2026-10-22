`init` is the only command that makes a store. It takes the directory as its argument and no
`--repo`: a `--repo` given beside it, or `SWARMOPS_REPO`, is not read, and `meta.repo` in the
envelope is DIR as you wrote it. It needs no identity, and it has no MCP tool: an MCP server
serves a store that exists already.

### What it creates

DIR is created if it is missing, with any missing parent directory. A directory that exists may
hold other files: `init` adds its own beside them. The store is these four entries:

| Entry | What it holds |
| --- | --- |
| `FORMAT` | The layout's version, `swarmops-store 1`. |
| `REPO` | The repository's id, 32 hex digits chosen at random, then its name. |
| `objects/` | The 256 fan-out directories of the loose objects, and `tmp/`. |
| `meta/` | The metadata store: the journal, the index and the refs. |

`FORMAT` is written last. A store is whole once `FORMAT` exists, and a crash or a failed sync
before that leaves no `FORMAT`: other commands refuse the directory, and `init` run again on it
completes the store, with a new repository id. Two `init`s of one directory at once are not
supported.

The new store holds no changeset and no ref, so `main` does not exist yet. The first commit
creates it with `--advance main`; [Start a store's
history](../../../guides/commit-and-land.md#start-a-stores-history) shows how.

### The name

`--name` is kept verbatim: any UTF-8 of at most 255 bytes, the empty string included. Without
it, the name is DIR's last component; for `.` or `..`, which have none in the path, it is the
name of the directory they lead to. The name is a label: `info` prints it, and no command looks a
store up by it.

### Patterns

- One store per repository, at a path every agent and script is given, through `--repo` or
  `SWARMOPS_REPO`. There is no default location: [Create a
  store](../../../operate/stores.md#create-a-store) uses `~/.local/state/swarmops/repos/<name>`
  as a convention only.
- Set the directory's permissions before agents use it: [Set the
  permissions](../../../operate/stores.md#set-the-permissions).
- To start from a git history instead of an empty store, run `init`, then
  [`git import`](../../commands/git-import.md).
- Check a new store with [`info`](../../commands/info.md) or
  [`verify`](../../commands/verify.md).

### Failures

- A directory that holds a store already (a `FORMAT` file) is `E_EXISTS` (exit 1), and nothing
  is written; the hint is `swarmops --repo <dir> info`.
- A name of more than 255 bytes is `E_INVALID_ARG` (exit 2) with `details.store_limit`
  `repo_name_bytes`, `max` and `got`, refused before anything is created.
- A path that leads under `/mnt/`, where WSL mounts Windows drives, is `E_UNSUPPORTED_FS`,
  refused before anything is created. [Where a store may
  live](../../../operate/stores.md#where-a-store-may-live) lists the filesystems a store is
  tested on.
- `.` or `..` whose directory has no name gives `E_INVALID_ARG`, with the hint to pass
  `--name`.
- A filesystem that refuses a write is `E_IO`, with the failing `fs_op` in `details`.
