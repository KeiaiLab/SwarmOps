`diff` compares the trees of any two changesets and lists the files that differ, in path order,
a page at a time. `--format` decides how much it says of each file, from the path and its status
up to unified hunks, so an agent can ask for exactly what it needs.

### What differs

FROM is the old side, TO the new; both resolve in one snapshot, and the result names the full
ids they resolved to. The two need not be related: any two changesets compare. Swapping them
swaps `added` and `deleted`. The same changeset on both sides gives `files: []`.

Only files, executables and symlinks are listed, never a directory. Each has a `status`:

| `status` | Meaning |
| --- | --- |
| `added` | In TO only: in FROM nothing, or a directory, is at its path. |
| `deleted` | In FROM only. |
| `modified` | Its content or its executable bit changed: a file or an executable on both sides, or a symlink on both sides. |
| `type_changed` | A symlink on one side, a file or an executable on the other. |

A file that becomes a directory is that file `deleted` and the directory's files `added`. No
rename is detected: a moved file is one `deleted` and one `added`.

### Formats

| `--format` | Adds to `path` and `status` | Reads content |
| --- | --- | --- |
| `names` (default) | Nothing. | No: trees only. |
| `stat` | `binary`; for text, `insertions` and `deletions` in lines. | Both sides of each listed file, whole. |
| `unified` | Also `old_kind`, `new_kind`, `old_blob`, `new_blob` and, for text, `hunks`. | The same. |

A side is binary when it is not UTF-8, or holds a NUL in its first 8,000 bytes; a binary file
gets no line counts and no hunks. A change of the executable bit alone, or of the kind with the
same bytes, is `modified` or `type_changed` with no hunks (`hunks: []`) and 0 lines either way.
A side of more than 49,152 lines, or of more than 8 MiB, is not line-diffed: no line counts, and
`hunks_truncated: true`.

Each hunk carries the numbers of a `@@ -old_start,old_lines +new_start,new_lines @@` header and
its lines in order, each `context`, `remove` or `add`, the removed lines of a change before the
added ones; a line keeps its `\n`, and a `\r` is part of its line. `--context` (3 by default) sets
the unchanged lines around each change; `--context 0` gives the changes alone.

### Paths

`--path P` keeps the files at or below P: a file's own path takes that file, and a directory's
everything below it. Paths match whole components (`--path Zeta` does not take `Zeta.txt`), one
trailing `/` is taken, and `--path` again adds more. An empty path is `E_INVALID_ARG`. Only the
directories a path reaches are read, and a subtree both sides share is never read at all.

### Pages

`--limit` (500 by default, at least 1) caps the files of a page. A `unified` page is also held
within `--max-bytes` (32 KiB by default, at least 1) of its files as JSON: it stops before the
first file that would take it past that, but always holds one file, whose hunks are cut to the
first ones that fit, with `hunks_truncated: true`. Its `insertions` and `deletions` still count
every line. `names` and `stat` pages ignore `--max-bytes`.

A cut page says `truncated: true` and gives `cursor`, the path of its last file; `--cursor`
starts the next page after that path. A file whose hunks were cut is not sent again on the next
page: to see all of it, diff it alone (`--path <file>`) with a larger `--max-bytes`, or read both
blobs with `cat`. A cursor that is not a repository path is `E_INVALID_ARG`; one that names no
listed file starts after where it would sort.

### Patterns

- **What one changeset changed.** `diff X~1 X` lists the changes of X against its first parent,
  a page at a time, which `show` caps without a cursor. The first changeset of a history has no
  parent to diff against: `show` lists its changes.
- **What a change does since its base.** `diff <base> <head>` of an agent's work shows what a
  `land` would publish, before the merge with what the target gained.
- **Cheap first, then detail.** A `names` page reads no content; ask for `unified` with `--path`
  for only the files you need to read.
- **Pin both sides.** Give full ids, the `from` and `to` of the first page, to every later page:
  the cursor is a path and does not pin the revisions, so pages of `main` can come from two
  revisions if another agent lands in between.
- **Through MCP**, the tool is `diff`, with `from`, `to`, `paths` (an array), `format`,
  `context`, `limit`, `cursor` and `max_bytes`. Ask for 100 files or fewer a call, and
  `max_bytes` of 32,768 or less for `unified`: see
  [Result size limits](../../../develop/mcp.md#result-size-limits).

### Caveats

- `stat` and `unified` read both versions of every listed file whole, and build its hunks before
  the page is cut to `--max-bytes`, so memory and time follow the files, not the page. `names`
  reads no content. See [known limitations](../../../known-limitations.md#reads-and-response-sizes).
- `--limit 0` and `--max-bytes 0` are `E_INVALID_ARG`.

### Guides

- [Reading and repairing history: Compare two revisions](../../../guides/history-and-undo.md#compare-two-revisions)
- [Reading and repairing history: Page through a large diff](../../../guides/history-and-undo.md#page-through-a-large-diff)
- [MCP integration: Result size limits](../../../develop/mcp.md#result-size-limits)
