`info` opens the store, reads its metadata in one snapshot and prints what the store is and
holds. It reads only: it needs no identity, takes no lock and writes nothing. Its MCP tool is
`repo_info`, which takes no arguments.

### What it reports

- `repo_id` and `name` come from the store's `REPO` file, fixed at [`init`](../../commands/init.md).
- `store_version` is the layout version the store's `FORMAT` file names, 1 for every store this
  release makes.
- `changesets` counts every changeset the store holds, those no ref reaches included: a commit
  that was never landed counts.
- `refs` counts the refs, `main` and every `release/<name>`.
- `main` is the full id of the changeset `main` names. The field is left out until `main`
  exists, so a script tells an empty store by its absence.

All five come from the same snapshot, so they agree with each other at one moment. Another
writer may move `main` right after: to build on `main`, read it once and pass that id as
`commit --parent`, as [Publish a change](../../../guides/commit-and-land.md#publish-a-change)
does with `resolve main`.

### Patterns

- A health check: `info` exits 0 exactly when the store opens and its metadata reads. Run it
  before handing a store to agents, or from a monitoring job.
- Find the store a script will use: `meta.repo` says which store the command opened, as you
  named it in `--repo` or `SWARMOPS_REPO`.
- Count growth: `changesets` rises with every commit and every land, and never falls, since the
  store removes nothing yet ([Monitoring](../../../operate/monitoring.md) lists what else to
  watch).

### Failures

- No store named, neither `--repo` nor `SWARMOPS_REPO`, or an empty `--repo`, is
  `E_NOT_A_REPO` (exit 6). An empty `--repo` does not fall back to `SWARMOPS_REPO`.
- A directory without a `FORMAT` file is `E_NOT_A_REPO`, with the hint `swarmops init <dir>`; a
  `FORMAT` that names another layout version is `E_NOT_A_REPO` too, the store being of a layout
  this build does not read ([Upgrade](../../../operate/install-and-upgrade.md#upgrade)).
- A `SWARMOPS_REPO` that is not UTF-8 is `E_INVALID_ARG` (exit 2), with `details.source` naming
  the variable.
- A damaged metadata file is `E_STORE_CORRUPT`, with the hint `swarmops verify`; a store under
  `/mnt/` is `E_UNSUPPORTED_FS`.
