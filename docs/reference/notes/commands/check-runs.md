`check runs` lists the run lines of a workspace's or a directory's checks, newest first: what
ran lately, how long each check waited and ran, and how it ended. It reads only: no identity, no
lock, no store operation, and it writes nothing.

### What a run line is

Each check a run of [`check run`](../../commands/check-run.md) reaches appends one line of
diagnostics, a JSON object, to `run-lines.jsonl` on the host that ran it; that page has the keys,
where the file is and how it rotates. A line is not a record of the store: the file is not
synced, anything that can write the directory can write it, and nothing decides from it. No gate
and no land reads a run line, and a line says nothing a later check may rest on. From M33-1 the
store's own run records are to be what this command reads; `source` says which it read, and is
`run_lines` today.

### Which lines

- `check runs` with `--ws NAME`, `--ws-path DIR`, or the workspace around the working directory:
  the lines in that workspace's `.swarmws/check-runs/`. The workspace is found as the `ws`
  commands find it, with no store opened. `vouched` is `false`.
- `check runs --dir DIR`: the lines beside the runs of `DIR`, below the runs root, as `check run
  --dir` keeps them. `vouched` is `true`, and the envelope has no `meta.repo`.
- `--run-lines DIR`, else `SWARMOPS_CHECK_RUN_LINES_DIR` (absolute; else `E_INVALID_ARG`,
  `details.reason: run_lines_dir`): the lines in that directory, whatever run wrote them there,
  of every directory and workspace that sent its lines to it. `vouched` is `true`. Something
  there that is no directory is `run_lines_dir` too.

`dir` names the directory read, absolute. Both files there are read, `run-lines.1.jsonl` and
`run-lines.jsonl`, each opened before either is read and never through a symbolic link. A file
that is not there holds no line: a workspace or directory that has run nothing answers an empty
page, exit 0. A file that does not open, or is anything but a regular file, is `E_IO` with
`details.path`.

### Filters

A line is listed when it matches every filter given.

| Flag | Request | Matches | Refused (`E_INVALID_ARG`, `details.reason`) |
| --- | --- | --- | --- |
| `--check NAME`, repeatable | `checks` | A line whose check is one of them. | `check`: not a check's name. |
| `--tree TREE` | `tree` | A line whose tree starts with these 12 to 64 lowercase hex digits. A line of the directory form, which has no tree, never matches. | `tree` |
| `--outcome OUTCOME`, repeatable | `outcomes` | A line that ended as one of `passed`, `failed`, `timed_out`, `cancelled`, `not_started`. | The command line takes no other word; the tool refuses `not_run`, which leaves no line, with `outcome`. |
| `--run RUN` | `run` | The lines of that run. | `run`: not a run's id. |
| `--since MS`, `--until MS` | `since`, `until` | A line whose check started at or after `since` and before `until`, in Unix milliseconds. | `time`: `since` after `until`. |

### Order, pages and the cursor

Lines come newest first by when each check started (`at`), then by run, check and attempt: the
order is the lines' own, not the files', so lines that several runs appended at once, and a
rotation, change no page. Two lines of the same check, run, attempt and time are listed once.

A page holds at most `--limit` lines (`-n`; 1 to 1,000, 100 by default; another is
`details.reason: limit`). When more lines match, the page carries a `cursor`: give it back as
`--cursor`, with the same filters, for the page after. A page with no `cursor` is the last. Pass
a cursor back as it was returned and do not build one: it names a place in the order, not a
position in a file. Lines appended since the first page appear only on a new first page; lines
rotated away are no longer listed; a later page repeats nothing and loses nothing that is still
kept. A cursor pins no filter: with other filters it goes on from the same place under them. One
that no page gave is `details.reason: cursor`.

### Lines that do not read

A line longer than 4 KiB, one that is not UTF-8 or not JSON, one that is not a run line or whose
values a run does not write (a run that is no id, a name that is no check's, a tree that is not
64 hex digits, `not_run`, a run directory that is relative or past 1,024 bytes), and a last line
with no line end (a writer in the middle of its append) are passed over and counted in
`skipped`. `scanned` counts every line read, matching or not.

### Bounds

Each page reads both files whole, at most 128 MiB, as a stream: memory holds one line and the
page, never a file. How long a page takes on full files is not measured.

### The MCP tool

`swarmops mcp` serves the workspace form as the tool `check_runs`, whose arguments are
`CheckRunsReq` (`workspace`, `checks`, `tree`, `outcomes`, `run`, `since`, `until`, `limit`,
`cursor`) and whose result is the command's. It reads the directory the server's
`SWARMOPS_CHECK_RUN_LINES_DIR` names, else the workspace's own state; there is no directory form
there.
