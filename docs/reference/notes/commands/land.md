`land` is how agents publish to a shared ref. It takes a change, merges it three-way onto
whatever the target ref holds now, and publishes the result there as one new changeset, the
ref's move with it, in one transaction. Nothing is recorded unless the whole land succeeds.
[Commit and land](../../../guides/commit-and-land.md) is the guide; this section is the
command's behaviour in full.

### What a land publishes

The change is HEAD with its first-parent chain after the base:

```text
        base ── c1 ── c2 ── HEAD         the change: c1, c2 and HEAD
          │
  main: base ── t1 ── t2                 what main gained since the base
                       │
                       └── landed        one new changeset, parent t2
```

- `--base` defaults to the merge base of HEAD and the target's head, along first parents. Given,
  it must be a first-parent ancestor of HEAD; it need not be on the target.
- The chain is squashed: `landed` holds what the chain changed merged with what the target
  gained since the base, its parent is the target's head (`rebased_onto`), and the target stays
  linear. `squashed_from` lists the chain, oldest first.
- The changeset's author is HEAD's author and its committer is you. Its message is HEAD's unless
  `-m` gives one. Its metadata keeps HEAD's `agent` and `run_id`, and adds `lander` (your agent),
  `lander_run_id` (your run id, when you have one), `land_key` (with `--key`) and
  `squashed_from`. [`show`](../../commands/show.md) prints them.
- The target's move is recorded for your agent; [`op log`](../../commands/op-log.md) lists the
  land as one operation. [`op undo`](../../commands/op-undo.md) takes back a land on a
  `release/<name>` ref; on `main` it is refused (`E_REF_PROTECTED`): land a change that reverts
  it instead.

### The target

`--target` is `main` (the default) or `release/<name>`, and it must exist: a name of another
form is `E_REF_NAME_INVALID` (exit 1), and a ref that does not exist is `E_REV_NOT_FOUND`
(exit 6). [Land on a release ref](../../../guides/commit-and-land.md#land-on-a-release-ref)
shows a release ref.

### When the target moves meanwhile

A land reads the target, merges, and publishes by compare-and-swap. When another writer moved
the target in between, the land merges again onto the new head and tries again, up to
`--max-attempts` times (8 by default, 1 to 32). `attempts` says how many it took. `relocated`
says the publish found the target moved and put the change on the new head itself, without
another attempt. When every attempt loses, the land fails with `E_CAS_RETRY_EXHAUSTED` (exit 4,
retryable), `details.attempts` and `details.actual` set, and nothing recorded: run the same land
again. [Two agents landing at once](../../../guides/commit-and-land.md#two-agents-landing-at-once)
shows the race.

### Conflicts

A change that does not merge is `E_CONFLICT` (exit 3), and nothing is recorded. The target is
as the other writers left it. `details.report` names:

- `base`, `ours` (your change's head) and `theirs` (the target's head it was merged with), by
  full id;
- each conflicting file: its `path`, its `kind`, the blob ids of the sides that hold it, its hunks (each
  side's lines, numbered from 1, both ends included), and the target's changeset and agent that
  last changed it (`theirs_changeset`, `theirs_agent`);
- `kinds`: `content` (the same or adjacent lines of a text file), `binary`, `delete_modify`,
  `modify_delete`, `add_add`, `mode` or `type_change`;
- `auto_merged`, the paths both sides changed whose contents merged cleanly into a content
  neither side had.

Hunks carry their texts while their sum stays within `--max-conflict-bytes` (32,768 by default):
from the first hunk whose texts would pass it, no hunk carries its texts, and the report sets
`truncated`. The hunks' line positions and the blob ids stay, from which
[`cat`](../../commands/cat.md) reads any side. With `--max-conflict-bytes 0` a report carries no
hunk text. The files are bounded apart from it, at 24 KiB of entries without their texts
([conflict report entries](../../limits.md#conflict-report-entries)): the first file is always
there, its hunks past the bound as one hunk that spans them, without texts; a later file that
does not fit is left out, with every file after it, and `truncated` is set. The error's message
counts every conflicting path. The error's hint is the `cat` of the first conflicting path at
`theirs`, then the land again with `--resolve`.

### Resolving without a checkout

`--resolve FILE` (or `-` for stdin) lands again with a decision for each conflicting path:

```json
{"against": "<the report's theirs, full id>",
 "files": {"src/lib.rs": {"take": "theirs"},
           "src/new.rs": {"content": {"inline": "pub fn hi() {}\n"}},
           "docs/old.md": {"delete": true}}}
```

Each path takes exactly one of `take` (`ours` or `theirs`), `content` (with an optional `mode`,
the content in any form a commit's change takes) or `delete`, and decides that path and every
path below it. A resolution applies while the target holds its path as `against` did. When the
target changed the path again in the meantime, the land fails with a fresh `E_CONFLICT` whose
report names the path in `stale`: read the new `theirs`, and resolve against it. A conflict left
without a resolution fails the land the same way. [Resolve a
conflict](../../../guides/commit-and-land.md#resolve-a-conflict) walks through one.

### `--key`: a retry that is safe

A land key is 1 to 64 letters, digits and `-`, such as a uuid v7; anything else is
`E_INVALID_ARG`, before anything is read. With a key, the land first looks down the target's
first parents, to the change's base, for a changeset landed with the same key:

| What it finds | Result |
| --- | --- |
| A changeset squashed from this HEAD | `status: replayed`, the first land's result, nothing written. |
| A changeset squashed from another head | `E_INVALID_ARG` (exit 2), `details.key` and `details.landed`: the key was used for another change. |
| None | The land runs. |

Without a key, a land run again after it succeeded fails with `E_NOTHING_TO_LAND` (exit 1),
`details.reason` `no_changes`. A HEAD that is on the target already fails with `reason`
`on_target`. Make a new key for each change you land, and give it again only to retry that land
([Land keys](../../../guides/commit-and-land.md#land-keys)).

### Patterns

- Many agents, one trunk: each agent commits its change on the head it read, with `--parent`,
  and lands it with a key. Landings are serialised by the target's compare-and-swap; a
  non-conflicting race costs a re-merge, not a failure.
- On `E_CONFLICT`, an agent reads the report, decides each path, and lands again with
  `--resolve`; nothing needs to be undone, since nothing was recorded.
- On exit 4, run the same command again with the same key.
- Through MCP the tool is `land`, taking the same fields as `LandReq` (`swarmops schema
  LandReq`); there `resolutions.against` must be a full 64-hex id ([Landing through
  MCP](../../../develop/mcp.md#landing-through-mcp)).

### Other failures

- A base that is not a first-parent ancestor of HEAD, a HEAD and target that share no
  first-parent ancestor (give `--base`), and a chain that holds a merge changeset (more than one
  parent) are each `E_INVALID_ARG` (exit 2).
- `--max-attempts` outside 1 to 32 is `E_INVALID_ARG`, before anything is read.
- No identity is `E_IDENTITY_REQUIRED` (exit 5).
- `E_BUSY` (exit 4): another writer held the store's lock for the whole wait; nothing was
  recorded.

### Performance

A lost race re-merges only the paths the change touched onto what the target gained, rather
than the whole change from its base. Its cost is not measured yet ([a land's incremental
re-merge](../../../perf.md#m4--a-lands-incremental-re-merge-not-measured-yet)).
