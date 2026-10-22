`history` follows one path back through first parents and lists every changeset that changed
what it holds, newest first, each with what the path held right after it: the blob or tree id
and its kind, or nothing. It answers "who changed this file, and to what", including who
deleted it.

### What counts as a change

A changeset is listed when its own changes, against its first parent, touch the path:

- for a file, a new content, a new kind (a file made executable, a symlink made a file), its
  addition or its deletion;
- for a directory, any of those below it.

Each entry's `op` is `put` when something is at the path after the changeset, with its `kind`
(`file`, `exec`, `symlink` or `dir`) and its `id` (a blob id, or a tree id for a directory), and
`delete` when nothing is. An executable bit changed alone gives a `put` whose `id` did not
change, under a new `kind`. `author_agent`, `created_at` and `title` are what `log` gives for the
same changeset.

The path does not have to exist at REV. A file deleted on `main` keeps its history: the newest
entry is its deletion, the ones below its earlier versions, each `id` readable with `cat` at that
entry's changeset.

### The target

`REV:PATH` is required; the target splits at the first `:`, as `cat`'s does. One trailing `/` is
taken (`main:docs/` is `main:docs`). `REV` alone is `E_INVALID_ARG` ("names no path"), and so is
an empty path or one with a leading `/`. Paths match whole components: `main:src/lib` is not
`src/lib.rs`.

A path that no changeset on the first-parent history of REV touched is `E_PATH_NOT_FOUND`
(exit 6), on the first page only, with the hint of the `ls` of the directory it would be in.
`log --path` with the same path gives an empty page instead.

### Pages

`-n`/`--limit` (50 by default, at least 1) cuts the page; a cut page gives `cursor`, the id of its
last entry, and `--cursor` continues from that changeset's first parent. REV is not read when a
cursor is given, but the path is, so give the same `REV:PATH` on every page. A cursor at the
path's oldest change gives an empty page, not an error. A cursor is a full changeset id the
store holds; anything else is `E_INVALID_ARG`.

### Patterns

- **Blame a file.** `history main:src/lib.rs` lists who changed it and when; for one entry,
  `diff <changeset>~1 <changeset> --path src/lib.rs --format unified` gives the lines it changed.
- **Restore a deleted file.** The entry below the deletion holds the last content's blob id: read
  it with `cat <that changeset>:<path>`, or put it back with a commit whose change names the
  blob, `{"blob": "<id>"}`, which uploads nothing.
- **Watch a directory.** `history main:docs` lists the changesets that changed anything under
  `docs`, with the directory's tree id after each; two equal tree ids mean nothing below changed.
- **Through MCP**, the tool is `path_history`, with `rev` (`main` when omitted), `path`, `limit`
  and `cursor`. Ask for 100 entries or fewer a call: see
  [Result size limits](../../../develop/mcp.md#result-size-limits).

### Caveats

- History follows first parents only, as `log` does: a merge's other parents are never walked,
  and what a merge brought in shows as its own change. Native histories have no merges.
- No renames: a file moved elsewhere ends in a `delete` here, and its history at the new path
  starts with a `put`.
- `history` reads a derived value, the newest changeset on the chain that touched the path,
  which the first reader to ask for it derives: a walk back over the changesets that do not touch
  the path, stored with a memo every 64 changesets of a longer walk. The first history of a path
  on a long history pays that walk. `swarmops derive` does not derive these values ahead; a store
  that cannot be written answers all the same, without keeping them. A stored value that fails
  its checks is `E_STORE_CORRUPT`, with the hint `swarmops verify --deep`, and
  [making room](../../../operate/making-room.md#3-remove-the-derived-values) explains removing
  the derived values, which readers then derive again.

### Guides

- [Reading and repairing history: Follow one path](../../../guides/history-and-undo.md#follow-one-path)
- [Concepts: Derived value](../../../concepts.md#derived-value)
- [Known limitations: Commits, refs and history](../../../known-limitations.md#commits-refs-and-history)
