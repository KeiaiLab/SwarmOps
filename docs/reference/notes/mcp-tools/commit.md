A commit records a changeset from an explicit change list, on `parent` or else on main's head.

- **Advance a ref.** With `advance`, the same transaction moves that ref to the changeset, only if
  the ref still names `parent`; the server then needs `parent` as the full 64-hex id you read
  (`E_INVALID_ARG` otherwise). If the ref moved, nothing is recorded: `E_CAS_MISMATCH` with
  `details.committed` false. Check `result.advanced`: a server that does not know `advance`
  moves no ref.
- **Retry safely.** An `idempotency_key`, a uuid v7 new for each commit, makes the same call
  again a replay (`result.replayed`) instead of a second changeset.
- **Content.** A change carries its bytes inline or as base64, names a stored blob as
  `{"blob": id}` (see [`blob_put`](../../mcp-tools/blob-put.md)), or names a file as `{"file": path}`. The
  server reads that file itself, so the path must be absolute and name a regular file, not the
  server's own stdin or stdout; anything else is refused with `E_INVALID_ARG`.

To publish the changeset on `main`, [`land`](../../mcp-tools/land.md) it.
