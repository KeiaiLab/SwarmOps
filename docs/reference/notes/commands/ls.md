`ls` lists a directory at any revision without a checkout: names, kinds, sizes and object ids,
one page at a time. With `--depth` it walks the directories below as well, so one call can map a
whole subtree.

### What the target names

`REV` alone, or `REV:` with an empty path, lists the root. `REV:PATH` lists the directory at
PATH, and one trailing `/` is taken (`main:src/` is `main:src`); a second one is
`E_INVALID_ARG`. The target splits at the first `:`, as `cat`'s does.

A file at PATH is `E_PATH_NOT_FOUND` (exit 6), with the message "is a file at REV, not a
directory" and the hint `swarmops cat REV:PATH`; nothing at PATH is `E_PATH_NOT_FOUND` with the
hint of the `ls` one level up.

### What each entry says

| Field | File, executable or symlink | Directory |
| --- | --- | --- |
| `name` | Its last path component. | Its last path component. |
| `path` | With `--depth` 2 or more: its path below the listed directory. | The same. |
| `kind` | `file`, `exec` or `symlink`. | `dir`. |
| `size` | The content's size in bytes; for a symlink, the length of its target. | Omitted. |
| `id` | The blob id, which `cat` and `diff` show too. | The tree id. |

A size comes from the object's header, so a listing reads no file content.

### Order, depth and pages

Entries come in pre-order: a directory, then the entries below it, down to `--depth` levels
(1, the default, lists the directory's own entries; 2 adds theirs; at most 64). Each directory's
entries are sorted by the bytes of their names, so `README.md` and `Zeta.txt` come before `bin`.

`--limit` (250 by default, at least 1) cuts the page. A cut page says `truncated: true` and gives
`cursor`, the `path` of its last entry; `--cursor` with that value starts the next page after it.
The cursor is a path relative to the listed directory, so the next call must name the same
target and the same `--depth`. A cursor that names no entry still works: the page starts after
where it would sort, so a cursor past every name gives an empty page. A cursor that is not a
repository path is `E_INVALID_ARG`.

`--depth 0`, `--depth 65` and `--limit 0` are `E_INVALID_ARG`.

### Patterns

- **Map a subtree.** `ls REV:src --depth 64` lists everything below `src`, a page at a
  time; each entry's `path` is ready to pass to `cat REV:src/<path>`.
- **Find what changed by id.** Two listings of the same directory at two revisions share every
  entry whose `id` did not change; a directory with an unchanged tree id holds nothing new below
  it. `diff` does this comparison for you, file by file.
- **Pin the revision for paging.** Resolve `main` to a full id first and list that: the cursor
  does not pin a revision, so pages listed at `main` can come from two revisions if another
  agent lands in between.
- **Through MCP**, the tool is `read_tree`, with `rev`, `path` (omitted for the root), `depth`,
  `limit` and `cursor`. Ask for 100 entries or fewer a call: see
  [Result size limits](../../../develop/mcp.md#result-size-limits).

### Caveats

- Paging limits what is returned, not what is read: a page decodes the whole tree object of the
  listed directory, and with `--depth` of every directory it enters. A very wide directory costs
  its whole tree on every page. See
  [known limitations](../../../known-limitations.md#reads-and-response-sizes).
- A directory exists only while something is below it: there are no empty directories to list.
  A file that becomes a directory, or the other way, shows as a new `kind` at the same name.

### Guides

- [Reading and repairing history: List a directory](../../../guides/history-and-undo.md#list-a-directory)
- [MCP integration: Result size limits](../../../develop/mcp.md#result-size-limits)
