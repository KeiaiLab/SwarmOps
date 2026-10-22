`log` walks history from a revision back through first parents, newest first, one page at a
time, and gives each changeset's id, parents, agent, time and title. With `--path` it keeps only
the changesets that changed the paths you name.

### Where it walks

From REV, `main` when it is omitted, back through each changeset's first parent to the first
changeset of the history. REV can be any revision, so `log <changeset>` walks an agent's commit
that has not landed, back into the history it was made on. A merge's other parents are never
walked. Each entry's `title` is the first line of the message, and `created_at` the committer's
time in Unix milliseconds.

Without `--path`, every changeset on the walk is listed, an empty one (`commit --allow-empty`)
among them.

### Paths

`--path P` keeps the changesets whose own changes, against their first parent, touch P: a file
at P (its content, its kind, or whether it is there) or anything below a directory P. Paths match
whole components, so `--path src/lib` matches neither `src/lib.rs` nor `src/library/`. One
trailing `/` is taken. Give `--path` again for more paths: a changeset that touches any of them
is listed once. A path that no changeset touched gives an empty page, not an error; an empty path
or one with a leading `/` is `E_INVALID_ARG`.

With `--path`, a changeset with no changes is never listed.

### Pages

`-n`/`--limit` (50 by default, at least 1) cuts the page. A page with more history after it says
`truncated: true` and gives `cursor`, the full id of its last entry. `--cursor` with that id
continues from that changeset's first parent:

- REV is not read when a cursor is given: the cursor alone says where the page starts.
- The paths are not in the cursor. Give the same `--path` options on every page of a filtered
  log; a page without them lists every changeset from there on.
- A cursor is a full changeset id the store holds; a prefix or anything else is `E_INVALID_ARG`.
- A cursor at the first changeset of a history gives an empty page.

A cursor is a changeset id, so the pages of one log stay on the history the first page started,
whatever lands meanwhile.

### Patterns

- **What landed since I last looked.** Keep the newest id you have seen; read `log` pages from
  `main` until that id appears.
- **Who changed this area.** `log --path src/api --path docs/api` lists the changesets that touched
  either directory, with their agents; `show` of one gives its files.
- **See an agent's chain.** `log <changeset>` of an agent's commit lists its first-parent chain
  and the history it was made on below it; the entries above the first one `main`'s history also
  holds are the commits a `land` of it would squash.
- **Through MCP**, the tool is `log`, with `rev`, `limit`, `cursor` and `paths` (an array). Ask
  for 100 entries or fewer a call: see
  [Result size limits](../../../develop/mcp.md#result-size-limits).

### Caveats

- History is first-parent only: what a merge brought in from its other parents shows as the
  merge's own change, and the changesets on those parents are never listed. Native histories have
  no merges (`main` is linear, and lands squash); histories brought in by `git import` can.
- No renames are detected: a file moved from A to B touches both A and B.
- `--path` reads a derived value, the newest changeset on the chain that touched the path. The
  first reader that asks for it derives it: it walks back over the changesets that do not touch
  the path and stores the result, with a memo every 64 changesets of a longer walk, so that a
  later walk from anywhere on that stretch takes at most 64 steps. So the first filtered log of a
  path on a long history pays that walk; a later one reads the stored values and walks only what
  they do not cover. `swarmops derive` does not derive these values ahead, since they are per
  path. A store that cannot be written answers all the same, without keeping them.
- A derived value that fails its checks is `E_STORE_CORRUPT`, with the hint
  `swarmops verify --deep`.

### Performance

A plain log reads each changeset's index record and object: in the v0.1.0 measurement 100
first-parent steps took 419 µs on a chain of 200,000 changesets and 479 µs on 1,000,000, and
`log -n 5` gave its 5 entries in 0 ms on a store of 32,101 changesets. See
[the performance ledger](../../../perf.md#v010--just-perf-in-a-quiet-window-2026-10-21).

### Guides

- [Reading and repairing history: Walk the history](../../../guides/history-and-undo.md#walk-the-history)
- [The daily loop: Look at what landed](../../../operate/daily-loop.md#3-look-at-what-landed)
- [Concepts: Derived value](../../../concepts.md#derived-value)
