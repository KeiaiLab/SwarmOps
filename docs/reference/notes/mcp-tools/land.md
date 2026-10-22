It merges `head`, with its first-parent chain since `base`, three-way onto `target` (`main` by
default) and publishes the result as one changeset in one transaction.

On `E_CONFLICT`, `details.report` lists each conflict.
Resolve them in `resolutions`, decided `against` the report's `theirs`, its full 64-hex id: the
server refuses a ref name, a relative revision or a prefix there (`E_INVALID_ARG`).
A path the target changed since comes back in the report's `stale`.
A resolution's `{"file": path}` content follows the rules of [`commit`](../../mcp-tools/commit.md).

Give an `idempotency_key` to make a retry safe; run again with the same arguments, a land adds
nothing.
