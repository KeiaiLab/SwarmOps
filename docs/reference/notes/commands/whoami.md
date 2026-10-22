`whoami` prints the identity your mutations would carry, worked out exactly as `commit`, `land`
and the other mutations work it out, and does nothing else. It opens no store: `--repo` and
`SWARMOPS_REPO` are not read, and the envelope has no `meta.repo`. It has no MCP tool; an MCP
server's identity is the one its command line or environment gave it when it started ([Identity:
one per agent process](../../../develop/mcp.md#identity-one-per-agent-process)).

### Where the identity comes from

1. `--as <family>/<instance>`, when given.
2. Else `SWARMOPS_AGENT`.
3. `SWARMOPS_RUN_ID`, when set, is the run the agent acts in, whichever of the two named the
   agent.

A variable set to the empty string counts as unset. With neither `--as` nor `SWARMOPS_AGENT`,
the caller is anonymous: `{"auth": "anonymous"}`, with `agent` and `run_id` left out rather than
null. Reads work anonymously; the seven commands that change the store (`commit`, `land`,
`blob put`, `ref set`, `ref delete`, `op undo`, `git import`) refuse an anonymous caller with
`E_IDENTITY_REQUIRED` (exit 5).

### The agent name

`<family>/<instance>`: a family of `a-z`, `0-9` and `-`, then `/`, then an instance of letters,
digits, `.`, `_` and `-` that does not start with `.`, at most 255 bytes in all, such as
`agent/worker-17` or `operator/ada`. Anything else is `E_INVALID_ARG` (exit 2), its message
prefixed with where the name came from (`--as`, `SWARMOPS_AGENT`), which `details.source` holds
too. A variable that is not UTF-8 is refused the same way.

### What `auth` means

`trust` says the name is taken as claimed: this release verifies no identity, and any process
that can write the store's files may claim any name. The name is still recorded on everything
the caller writes: a commit's author and committer are the agent name with the email
`<instance>@<family>.swarmops.local`, its metadata holds `agent` and, when set, `run_id`, and
every ref move records the agent.

### Patterns

- Check the identity once, at an agent's start, before its first mutation:
  `swarmops whoami` exits 2 on a malformed name, and its `result.agent` should be the name the
  agent was given.
- Give every agent process its own instance, and a run id per task or job, so that
  [`op log`](../../commands/op-log.md), [`show`](../../commands/show.md) and
  [`log`](../../commands/log.md) tell their writes apart. [Say who you
  are](../../../guides/commit-and-land.md#say-who-you-are) and
  [Identities](../../../operate/stores.md#identities) give the conventions.
