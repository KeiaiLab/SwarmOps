Pass `max_bytes` of 32768 or less (less for a binary file, which comes back as base64): the
operation's own default is 1 MiB, and a result holds the content twice.
While the result is truncated, call again with `offset` set to its `next_offset`.
`lines` reads a range of lines, numbered from 1, both ends included, instead of an `offset`;
giving both is `E_INVALID_ARG`.

`read_file` is the one tool that raises Claude Code's result size limit for itself, through its
`_meta` in `tools/list`: see [Result size limits](../../../develop/mcp.md#result-size-limits).
