It stores content as a blob, outside any commit, and gives its id; a commit's change then names
it as `{"blob": id}` instead of carrying its bytes, the way to commit a large file.
`written` is false when the store held the blob already, or a concurrent writer placed it first.
A `{"file": path}` content follows the rules of [`commit`](../../mcp-tools/commit.md): an absolute path to a
regular file.
A hint that names `swarmops blob put` means this tool.
