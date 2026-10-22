# Monitoring

What to collect from a swarm and what to read in it.
Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages).

## Before you start

- The agents' envelopes: the [wrapper's files](agents.md#keep-a-copy-of-every-envelope), one per
  agent, or the client's transcripts.

## Summarise the envelopes

Every command and every tool result is an envelope, so a log of envelopes is the monitoring data. A
command prints its envelope; the MCP server keeps none of its answers, so what agents' calls did is
read from the files that [the wrapper](agents.md#keep-a-copy-of-every-envelope) keeps, one per agent, or from the
client's transcripts. A line of those files holds its envelope twice, as an object and, escaped, as
text; these commands read the object, which has no escaped quotes, and need no `jq`. In the
wrapper's directory:

```sh
cd ~/.local/state/swarmops/envelopes
for f in *.jsonl; do
  printf '%-22s calls %s, failed %s, recovered %s, warnings %s\n' "${f%.jsonl}" \
    "$(grep -c '"structuredContent"' "$f")" "$(grep -c '"isError":true' "$f")" \
    "$(grep -c '"recovered"' "$f")" "$(grep -c '"warnings":' "$f")"
done
cat *.jsonl | grep -o '"code":"E_[A-Z_]*"' | sort | uniq -c | sort -rn
cat *.jsonl | grep -o '"elapsed_ms":[0-9]*' | cut -d: -f2 | sort -n | tail -3
```

```text
agent_worker-1         calls 7, failed 1, recovered 0, warnings 0
agent_worker-2         calls 6, failed 2, recovered 0, warnings 0
      1 "code":"E_PATH_NOT_FOUND"
      1 "code":"E_CONFLICT"
      1 "code":"E_CAS_MISMATCH"
19
23
31
```

The first command counts, for each agent, the calls answered, the failed ones, the writes that
repaired what a dead writer left (`meta.recovered`) and the results that carry `warnings`; the
second counts the failures by code; the third lists the three slowest calls by `meta.elapsed_ms`.


## What to read in them

### Errors by code

Expected now and then, and handled by the agent: `E_CONFLICT` (exit 3),
`E_CAS_MISMATCH` (re-read, then land). A rising count of `E_BUSY` or `E_CAS_RETRY_EXHAUSTED`
(exit 4) means more contention than the lock serves: too many writers, a slow disk, or something
holding the lock (a backup). Act at once on `E_IO`, `E_IO_TRANSIENT` (their `details` name the
file operation and the store file), `E_STORE_CORRUPT` and `E_INTERNAL`, and on an MCP server
that exits.

### `meta.recovered`

`meta.recovered` on a write: what it repaired of a writer that died mid-write. Agents get
killed, so it appears now and then; often, it means something keeps killing writers.

### `warnings`

`warnings` in a result: what an operation that succeeded still wants you to know, such as a
commit that is durable but whose follow-up work failed (the next write finishes it), or an export
that wrote commits anew because their history was imported with a loss.

### `meta.elapsed_ms`

`meta.elapsed_ms` of commits and lands: the lock's wait is in it.

### `verify --deep`

`problems` and `findings` empty, on a schedule and on every backup.

### Disk

Free space on the store's filesystem, and the size of `objects/`, `objects/tmp/` and
`meta/derived/`, which only grow. `meta/derived/` grows with the exports: the first export of a
history writes one file of derived git ids per object it exports, about as many files as
`objects/` holds, each taking a whole filesystem block (4 KiB on ext4) for its 72 or 73 bytes. At
470 commits the store took 57 MB in 8,854 files after the import, and 92 MB in 17,232 files after
the first export.

The store keeps one file per object and one per derived value, so count its files and the
filesystem's inodes as well: ext4 fixes the number of inodes when the filesystem is made, and a
store of small files can use them up before it uses the space. [Making room](making-room.md) says
what may go.

```sh
find "$SWARMOPS_REPO" -type f | wc -l
df --output=iused,iavail,ipcent "$SWARMOPS_REPO"
```

```text
91
  IUsed     IFree IUse%
3021323 241161717    2%
```

### The git side

Each export's `exported`, `branch_moved` and `rewritten` (0 unless the history
was imported with a loss), any `E_GIT_IMPORT_DIVERGED` or `E_GIT_EXPORT_DIVERGED` (someone wrote
the trunk, or an export waits for its review) or `E_GIT_WRITE`, how long the review branch has
been ahead of the trunk (`git -C "$GIT_SIDE" log --oneline stable..review` lists what waits), and
the CI runs on the review branch and the deployment runs on the trunk.

### `op log`

Who moved which ref, for moves no one expected.
