`expected_old` is the full 64-hex changeset id the ref held when you read it; the server refuses
a ref name, a relative revision and an id prefix there (`E_INVALID_ARG`), for the reasons
[`ref_set`](../../mcp-tools/ref-set.md) gives.
A ref that no longer holds it is `E_CAS_MISMATCH`, and the ref stays.
