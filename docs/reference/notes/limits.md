### Page sizes of 0

A `limit` or `max_changes` of 0, and a `max_bytes` of 0 for `read_file` or `diff`, is
`E_INVALID_ARG`: the schema states their minimum of 1 above. `derive`'s `limit` takes 0, and
then gives the journal's last frame as `next`.

### Contents past a limit

A change list holds at most 64 MiB and a commit's contents 1 GiB together; a blob named by id
counts toward neither. So store a large content first with `swarmops blob put`, which takes up
to 512 MiB, and name it in the change by its id: `{"blob": "<id>"}`. A request past a limit
writes nothing, and running it again fails the same way: `E_TOO_LARGE` is not retryable
([E_TOO_LARGE](../errors.md#e_too_large)).

### Over MCP

`swarmops mcp` runs each tool through the operation its command runs, so a tool meets the same
limits and errors. Only the request text differs: a message line past 64 MiB is a JSON-RPC
Invalid Request without an `id`, and the server reads on at the next line. The tool descriptions
ask agents for small pages, since clients limit long tool results
([Give agents the store](../../operate/agents.md#the-tools-agents-get)).

### Reads with no upper limit

`cat`'s `--max-bytes` (`read_file`'s `max_bytes`) has a default and no upper limit but the
file's size ([Known limitations](../../known-limitations.md)).
