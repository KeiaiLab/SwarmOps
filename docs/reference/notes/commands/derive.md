`derive` computes the store's eager derived values ahead of the readers that would otherwise
compute them, such as each changeset's first-parent skip list. A
derived value is a cache: computed from immutable objects, stored in a file of its own under
`meta/derived/`, never synced and named by no journal record ([Derived
value](../../../concepts.md#derived-value)). So `derive` writes nothing but that cache: it needs
no identity, it is not a mutation, and it has no MCP tool, being maintenance rather than an
agent's work.

Nothing is lost without it. Commits derive nothing, and a read that needs a value not derived yet
derives it and stores it. `derive` moves that work off the read path.

### Once, or following the journal

The journal records one frame per write transaction, a commit or a ref's move, numbered from 0
in the order they committed ([Journal and frame](../../../concepts.md#journal-and-frame)).

- Without `--follow`, `derive` reads the frames after `--after N` (from the first frame without
  it), at most 1,000 of them, derives the values of the changesets they record, prints one
  envelope and exits. `next` is the last frame it read: run it again with `--after <next>` to go
  on through a longer journal, or later to take what was recorded since. `next` is left out when
  no frame follows `--after`.
- With `--follow`, it runs until it is killed. It starts at the journal's end, so it derives
  only what is recorded from then on; `--from-start` starts at the first frame instead, and
  `--after N` after frame N. Once it has read every frame it waits `--interval-ms` (500 by
  default) and looks again. It prints one envelope for each run that derived something, at once,
  and nothing for a run that derived nothing.

`--after` and `--from-start` exclude each other, and `--interval-ms` and `--from-start` need
`--follow`; any of these on a command line the grammar refuses is `E_INVALID_ARG` (exit 2).
`--interval-ms` is at least 1.

### Reading the counts

`derived` counts the values this run computed for the changesets its frames record; `cached`
counts those the store held already. Values computed on the way, an older changeset's that a
newer one needed, are stored but not counted. Run twice over the same frames, the second run
derives nothing and counts the values as cached.

### Patterns

- Beside a busy store, run `swarmops derive --follow` as a long-lived process, so that agents'
  reads find their values stored. Its output is one compact JSON envelope a line when stdout is
  not a terminal, which a log collector can read as it comes.
- After removing `meta/derived/` to make room, run `swarmops derive` once to compute the eager
  values for every changeset again ([Derive them
  again](../../../operate/making-room.md#4-derive-them-again)).
- To go through a large journal in steps, loop on `--after <next>` until `next` is left out.

### Caveats

- A `--follow` run that fails ends the command with that run's envelope and its exit code; a
  store that does not open ends it at once the same way. Output that can no longer be written
  (the reader of its stdout gone) ends it with exit 1 and a line on stderr.
- `--after` past the journal's last frame is not an error: there is nothing after it, so the run
  derives nothing and leaves `next` out.
