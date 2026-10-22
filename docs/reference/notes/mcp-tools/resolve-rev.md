`rev` is a ref name, a full 64-hex changeset id or a unique prefix of 12 or more hex digits, then
any `~N` suffixes, resolved as every tool resolves one.
`kind` is `ref` when the revision is exactly a ref's name, whose target moves with the ref, and
`changeset` otherwise.

Its `id` is the full id that `ref_set` and `ref_delete` take as `expected_old`, and `commit` as
`parent` with `advance`: read the ref's head here, then name that id, so that the
compare-and-swap protects what you read.
