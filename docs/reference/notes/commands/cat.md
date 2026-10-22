`cat` reads one file at any revision without a checkout: the whole file, the bytes from an
offset, or a range of lines, always within `--max-bytes`. It is the read an agent makes before it
edits a file, and the one it pages through a large file with.

### What the target names

`REV:PATH` splits at the first `:`, so a path may hold `:` itself (`main:a:b` is path `a:b`).
PATH is a repository path: UTF-8, relative to the root, `/`-separated, with no leading or
trailing `/`, no empty, `.` or `..` component, and no NUL. `main:/src/lib.rs`, `main:src/` and
`main:./README.md` are `E_INVALID_ARG`; `ls`, `history`, `log --path` and `diff --path` take one
trailing `/`, but `cat` does not.

The entry at PATH decides the result:

| At PATH | Result |
| --- | --- |
| A file | Its bytes; `kind: file`. |
| An executable | Its bytes; `kind: exec`. |
| A symlink | The link's target as its content, not followed; `kind: symlink`. |
| A directory | `E_IS_DIR` (exit 1), hint `swarmops ls REV:PATH`. |
| Nothing, or a path below a file | `E_PATH_NOT_FOUND` (exit 6), hint the `ls` of the directory PATH would be in. |

`blob` is the content's id and `size` the whole file's size, whatever part of it came back.

### The three ways to read

- **Whole file** (no option): the bytes from the start, at most `--max-bytes` (1 MiB).
- **`--offset N`**: the bytes from byte N to the end of the file, at most `--max-bytes`. An offset
  at or past the end gives empty content, `truncated: false`.
- **`--lines A-B`**: lines A to B, numbered from 1, both included. A line ends after its `\n`; a
  last line without one is a line too, and a `\r` belongs to its line. Only the lines of the
  range that exist come back: `lines` in the result says which, and `offset` where the first one
  starts. A range that starts past the last line gives empty content, no `lines`, and `offset`
  equal to the file's size.

`--offset` and `--lines` do not go together (`E_INVALID_ARG`, from the command line, or from the
operation when an MCP call sends both). `--lines` takes exactly `A-B`: `--lines 2` and
`--lines 0-2` are refused, and so is a range whose end comes before its start.

### Encoding and cuts

`content` is text when the bytes returned are UTF-8 (`encoding: utf8`), and standard padded
base64 otherwise (`encoding: base64`). A cut by `--max-bytes` keeps text as text: it moves back
to the start of the character it would split, so text can stop up to 3 bytes short of
`--max-bytes`, while bytes that are not text stop at exactly `--max-bytes`. An `--offset` that
falls inside a character makes the result base64, because those bytes are not text on their own.

`truncated: true` means the content stops before the end of what was asked for, and
`next_offset` says where it stops. The read on goes by bytes: `--offset <next_offset>` reads to the
end of the file, not to the end of a `--lines` range, so a paged line read must stop on its own at
the range's last line.

### Patterns

- **Read before you edit.** Read the file at a full id, and give that id to
  `commit --parent`. A commit without `--parent` builds on `main`'s head as it is when it runs,
  so a whole-file put made from an older read of `main` reverts what landed in between; `land`
  then merges the change onto the ref's head.
- **Page a large file.** Read with a `--max-bytes` the caller can hold, then repeat with
  `--offset` set to `next_offset` until `truncated` is `false`. Pin the revision to a full id first:
  `next_offset` does not pin it, so pages read at `main` can come from two revisions.
- **Read around a line.** `--lines 120-160` gives a window of a source file; `offset` in the result
  maps it back to bytes.
- **Compare content by id.** Two files with the same `blob` id hold the same bytes; `ls` and
  `show` give blob ids without reading content.
- **Through MCP**, the tool is `read_file`, with `rev`, `path`, `offset`, `lines` (an object,
  `{"first": A, "last": B}`) and `max_bytes`. Ask for 32,768 bytes or less a call: see
  [Result size limits](../../../develop/mcp.md#result-size-limits).

### Caveats

- `--max-bytes 0` returns empty content with `truncated: true` and `next_offset` equal to the
  offset it started from, so a loop that reads on while `truncated` never ends. Ask for at least
  one byte.
- `cat` reads the blob's object file whole, and verifies all of it, however little it returns:
  a large file costs its size in memory on every read.
- A file of control characters prints at about six times its size through the CLI, since JSON
  spells most of them in six bytes each, and at about thirteen times through MCP.
- These three are listed in
  [known limitations](../../../known-limitations.md#reads-and-response-sizes).

### Guides

- [Reading and repairing history: Read a file](../../../guides/history-and-undo.md#read-a-file)
- [Reading and repairing history: When the path is not a file](../../../guides/history-and-undo.md#when-the-path-is-not-a-file)
- [MCP integration: Result size limits](../../../develop/mcp.md#result-size-limits)
