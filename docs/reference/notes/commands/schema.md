`schema` prints the JSON Schema of the API's types: every request and response of every
operation, and the types they share, such as `Envelope`, `OpError`, `ChangeInput` and
`ErrorCode`. It is the machine-readable contract behind the JSON the other commands print and
the MCP tools take. It opens no store (`--repo` and `SWARMOPS_REPO` are not read, and the
envelope has no `meta.repo`), needs no identity, and has no MCP tool: an MCP client gets each
tool's input schema from `tools/list` instead.

### What it prints

- Without TYPE, `result` is an object with one entry per type, keyed by the type's name: 91 of
  them in this release.
- With TYPE, `result` is the same object with that one entry, so a script reads
  `result.<TYPE>` either way.

Each entry is a JSON Schema of draft 2020-12 (its `$schema` says so), with the types it uses
under `$defs`, so one entry stands alone. Descriptions are the API's own documentation, the
same text the Return information tables on these pages show.

### Which type to ask for

- A commit's change list: `ChangeInput` (one change), `ContentInput` (its content) and
  `ModeInput`; `CommitReq` is the whole request, as the MCP tool `commit` takes it.
- A land's resolutions file: `ResolutionsInput`, `ResolutionInput` and `Side`.
- A command's result: the type each page's Return information names, such as `CommitResp` or
  `LandResp`.
- The envelope around every result, and its errors: `Envelope`, `Meta` and `OpError`; the
  error codes: `ErrorCode`.

The CLI's flags map onto the request types' fields: `commit --key` is `CommitReq`'s
`idempotency_key`, and `land --resolve` reads a `ResolutionsInput`.

### Patterns

- Generate client types, or validate requests before sending them, from the full output.
- Check a change list against `ChangeInput` before `commit` runs it, rather than learn of a bad
  field from its `E_INVALID_ARG`.
- Find a type's name: an unknown TYPE fails with the list of every name in
  `details.types`.

### Failures

TYPE is matched exactly, case included: `commitreq` is not `CommitReq`. An unknown name is
`E_INVALID_ARG` (exit 2), with `details.name`, `details.types` and the hint `swarmops schema`.
