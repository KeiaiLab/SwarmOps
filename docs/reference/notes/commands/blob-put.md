`blob put` stores one file's bytes in the store as a blob, outside any commit, and prints the
blob's id. A later change names it by that id, `{"content": {"blob": "<id>"}}`, and the commit
then neither sends nor reads those bytes again. Its MCP tool is `blob_put`, which takes the
content inline, in base64 or as a file on the server's host.

### Why it exists

A change list is at most 64 MiB of text, and a commit's contents at most 1 GiB together. A blob
named by id counts toward neither: only one blob's own limit applies, 512 MiB. So large or many
files go in one `blob put` each, then one commit names them all. Over MCP, where every request
line is at most 64 MiB, it is also the way to send a file once and refer to it in several
commits.

### Behaviour

- FILE is read whole, by the command, on this host; `-` reads stdin to its end, whatever it is:
  a pipe, a file or a socket.
- The id is the content's hash: the same bytes always give the same id, whoever puts them. A put
  of bytes the store holds already writes nothing and says `written: false`; so does a put that
  a concurrent writer of the same bytes beat. `size` is the content's length in bytes.
- The blob is durable when the command returns.
- It is a mutation, so it needs an identity (`--as` or `SWARMOPS_AGENT`), but it records it
  nowhere: a blob has no author, no time and no metadata. The commit that names it records who
  wrote the file.
- It runs outside any transaction: it writes the object alone, takes no lock on the store's
  metadata and writes no journal frame, so [`op log`](../../commands/op-log.md) does not list it
  and there is nothing to undo. Until a commit names the blob, nothing in the metadata does.

### Patterns

```sh
BASE=$(swarmops resolve main | jq -r .result.id)
B=$(swarmops blob put big.bin | jq -r .result.blob)
printf '[{"path": "assets/big.bin", "op": "put", "content": {"blob": "%s"}}]' "$B" > change.json
swarmops commit -m "add the asset" --changes change.json --parent "$BASE"
```

- With many files, put them first, then commit one change list that names every id. The commit's
  `blobs_written` is 0 for each of them.
- Before putting a large file again, [`blob has`](../../commands/blob-has.md) tells whether the
  store holds it already.
- A blob nothing names stays in the store: nothing removes it, as nothing is removed yet
  ([Where a store may live](../../../operate/stores.md#where-a-store-may-live)).

### Failures

| Error | Exit | When |
| --- | --- | --- |
| `E_IDENTITY_REQUIRED` | 5 | No identity. Checked before the file is read. |
| `E_INVALID_ARG` | 2 | FILE cannot be read (`details.file` names it, `-` for stdin). |
| `E_TOO_LARGE` | 1 | The content is over 512 MiB (`details.scope` `put`), measured by the file's length before it is read; stdin is read no further than one byte past it. |
| `E_IO` | 1 | The object store could not be written; `details.fs_op` names the file operation that failed. |
