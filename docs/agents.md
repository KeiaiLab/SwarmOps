# For agents and programs

After this page a program or a coding agent can find every SwarmOps page, fetch it as Markdown, and
know which parts of a command's answer it may rely on across releases.

## Find the pages

The index of the documentation is at <https://swarmops.cc/llms.txt>: it lists the pages, each
linked to its Markdown.

## Fetch a page as Markdown

Every docs page is also served as Markdown, at its address plus `.md`:

```text
https://swarmops.cc/docs/guides/install     the page
https://swarmops.cc/docs/guides/install.md  its Markdown
```

The Markdown comes from the page's source file in the repository, under `docs/`.

## What a program may rely on

Every command prints one JSON envelope, and every MCP tool returns the same one
([CLI scripting, The envelope](develop/cli-scripting.md#the-envelope)). A program tests three
things in a failed envelope:

- `error.code`, the `E_*` code ([error reference](reference/errors.md));
- the exit code, the same as `error.exit`;
- `error.retryable`, which says whether running it again can succeed.

It never tests `error.message` or the wording of `hint`: they are text for a person, and their
wording may change in any release. The [versioning policy](policies/versioning-and-stability.md#internal-surfaces)
says so, and lists what stays compatible. A new code, tool or optional argument is not a breaking
change, so a program allows for codes it does not know: it reads their `exit` and `retryable`.

## Next

**Next:** [CLI scripting](develop/cli-scripting.md): exit codes, retry rules and idempotency keys,
with runs to copy.

- [MCP integration](develop/mcp.md): the same envelope as tool results, for agents that speak MCP.
