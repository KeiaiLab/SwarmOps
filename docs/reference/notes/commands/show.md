`show` prints one changeset whole: its place in the store, its parents and root tree, who wrote
it and who landed it, its message, the files it changed against its first parent, its metadata
and its git signatures. Use it to inspect a change before you land it, after it landed, or when
an error names one.

### Any changeset, on a ref or not

REV is any revision, so `show` reads changesets that no ref names: an agent's commit that has not
landed yet, or the `actual` of an `E_CAS_MISMATCH`. `seq` is the changeset's place in the store's
index, in commit order, counting every changeset the store holds; it is not a position on
`main`, and the `seq` values along `main` can skip numbers.

### Who and when

| Field | Holds |
| --- | --- |
| `author.agent` | The agent that wrote the change: the `agent` metadata, or `unknown/unknown` when there is none (as for a changeset `git import` made). `author.run_id` is the `run_id` metadata, when present. |
| `lander` | Only on a changeset a `land` made: the agent that landed it. |
| `created_at` | The committer signature's time, in Unix milliseconds. For a landed changeset that is when the land published it, not when its author committed. |
| `signatures` | The git author and committer, kept for git round trips; `secs` in Unix seconds. |

A land squashes the change it publishes into one changeset whose parent is the ref's head. Its
`author` is the change's author, `squashed_from` lists the changesets it squashed (oldest first,
ending at the head that was landed), and `land_key` is the `--key` of the land, when it had one.

### The changes

`changes` lists what changed against the first parent, sorted by path: `op` is `put` (with the
file's `kind` and `blob`) or `delete`. A change of an executable bit alone is a `put` whose
`blob` did not change. `--max-changes` (400 by default, at least 1) caps the list; `files_changed`
still counts every path, and `changes_truncated` says the list was cut. There is no cursor: to see
every file of a large changeset, `diff` it against its first parent (`diff REV~1 REV`) and page
the diff. A changeset made with `commit --allow-empty` has `changes: []` and `files_changed: 0`.

`metadata` holds the changeset's SwarmOps metadata as text, such as `agent`, `run_id`,
`commit_key`, `land_key`, `lander` and `squashed_from`; a value that is not UTF-8 comes in base64
under its key with `:b64` appended.

### Patterns

- **Review before landing.** `show <changeset>` of a commit an agent reports gives its files and
  blob ids, `cat <changeset>:<path>` reads any of them, and `diff <base> <changeset>` shows what
  the change did since the base it started from.
- **Trace a landed change back.** On `main`, `show` gives `lander`, `land_key` and
  `squashed_from`; `show` of each id in `squashed_from` gives the original commits.
- **Check a retry.** After an `E_CAS_MISMATCH`, `show <actual>` gives the winner's `agent` and
  `run_id` metadata: the way to tell another agent's change from your own earlier attempt.
- **Through MCP**, the tool is `show`, with `rev` and `max_changes`. Ask for 100 changes or fewer
  a call: see [Result size limits](../../../develop/mcp.md#result-size-limits).

### Caveats

- The changes of a merge are those against its first parent: what it brought in from its other
  parents shows as its own change. Native histories have no merges; histories brought in by
  `git import` can.
- `show` lists no renames: a moved file is a `delete` and a `put`.
- `--max-changes 0` is `E_INVALID_ARG`.

### Performance

`show` makes its reads in one snapshot. In the v0.1.0 measurement `show main` took 59.1 µs p50
with no ref but `main`, 149 µs with 16 more refs and 1.55 ms with 256: a snapshot reads every
ref file. See
[the performance ledger](../../../perf.md#v010--just-perf-in-a-quiet-window-2026-10-21).

### Guides

- [Reading and repairing history: Look at one changeset](../../../guides/history-and-undo.md#look-at-one-changeset)
- [Concepts: Land](../../../concepts.md#land)
- [Known limitations: Commits, refs and history](../../../known-limitations.md#commits-refs-and-history)
