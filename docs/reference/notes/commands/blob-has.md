`blob has` asks which of some blob ids the store holds, without reading any of their bytes. It
reads only: it needs no identity and writes nothing. Its MCP tool is `blob_has`, which takes the
ids as an array.

### Behaviour

- Each ID is a blob id in full, 64 hex digits, of either case. A prefix is not taken: only a
  revision resolves a prefix of a changeset id.
- Every ID is checked before the store is asked about any. One that is not 64 hex digits fails
  the whole command with `E_INVALID_ARG` (exit 2), `details.index` (from 0) and `details.blob`
  naming it.
- `present` lists the IDs the store holds a blob under, in lowercase, in the order first given,
  each once however often it was given. An ID the store does not hold is simply absent: a
  missing blob is not an error.
- An ID of another kind of object, a changeset or a tree, is never present, since it names no
  blob.
- It reads each object's header alone, so a blob listed as present has not had its bytes
  checked; [`verify --deep`](../../commands/verify.md) checks them.

### Patterns

- Before a commit that names blobs by id, check them: a change whose `blob` the store does not
  hold fails the commit with `E_INVALID_ARG`, naming the change, and records nothing.
- Before putting a large file, ask whether its id is there already, when you have the id from
  an earlier [`blob put`](../../commands/blob-put.md) or from a changeset's change list
  ([`show`](../../commands/show.md)).
- Compare the result with what you asked to find the missing ones: the ids you gave, lowercased,
  less `present`.
