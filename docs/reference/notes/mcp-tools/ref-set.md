Give `create` to make a new ref, or `expected_old` to move one: the full 64-hex changeset id the
ref held when you read it, from `ref_list`, `show`, `log` or a commit's result.
The server refuses a ref name, a relative revision such as `main~1` and an id prefix there
(`E_INVALID_ARG`): a ref name or a relative revision is read when the call runs, so the
compare-and-swap would protect nothing, and a prefix stops resolving once another changeset
shares it.
The input schema says the same with a 64-hex `pattern`.
The CLI's `ref set --expect` still takes any revision.

A ref that no longer holds `expected_old` is `E_CAS_MISMATCH`, and nothing moves: read the ref
again and decide.
