`check log` reads one check's log of a run of `check run`, on the host the run ran on, once or as
it grows; with `--failed`, in place of a check's name, it reads the logs of every check of the run
that did not pass. It reads only: nothing is written, no store operation is called, and the run's lock is
only tried, for an instant, on a descriptor opened to read, to tell a running run from one whose
runner is gone; a lock it cannot open or try (another account's run) is taken as held.

### Which run

`--run RUN` names a run by its id (the `run` of `check run`'s answer); else it is the newest run
that ran `CHECK`, and with `--follow` the newest of those still running first. The runs are those
of the workspace (`--ws`, `--ws-path`, or the one around the working directory, found as the `ws`
commands find it, with no store opened), kept in its `.swarmws/check-runs/`, or with `--dir DIR`
those of `DIR` below `SWARMOPS_CHECK_RUNS_DIR` or the state directory, as `check run --dir` kept
them. No run that ran `CHECK`, or a `--run` that is not there, is `E_INVALID_ARG` with
`details.reason: run`.

With `--failed` the run is `--run RUN`, else the newest run of the workspace or directory, by its
id, whether it still runs or has ended.

`--run-lines DIR`, with `--run RUN`, finds the run through its
[run lines](../../commands/check-run.md#run-lines) in `DIR` instead: it goes with neither `--dir`
nor `--ws`, and reads from any working directory. The newest line of that run names its run
directory, `run_dir`, which must be absolute and named as the run; the run is then read there as
any other, its record and its logs, for `CHECK` and for `--failed` alike. Only the flag does
this: `SWARMOPS_CHECK_RUN_LINES_DIR` does not change how `check log` finds a run. Refused, each
`E_INVALID_ARG`:

| `details.reason` | When |
| --- | --- |
| `run` | The lines hold no line of that run, or `RUN` is not a run's id. |
| `run_dir` | Its line names no run directory (a path past 1,024 bytes is left out of a line), or one of another name. The hint names `--dir` and `--ws`. |
| `run_gone` | The directory is not there any more: a host keeps the newest 20 runs of a directory or workspace. `details.failed` names the checks of the run that its lines say did not pass. |

### The log's format

UTF-8 text, a line of the log for each line of the check's output, each

```text
<seconds>.<milliseconds> <stream> <text>
```

the time since the check's start (seconds right-aligned in five places), the stream (`out` for
stdout, `err` for stderr, `run` for the runner's own line) and the line's text, without its `\n`
and one `\r` before it, bytes that are not UTF-8 replaced. Each stream's lines are in their own
order; the two streams are read apart, so lines of stdout and stderr written close together may
interleave otherwise than written. The runner's lines are `run <event> <JSON object>`:

| Event | When | Object |
| --- | --- | --- |
| `start` | The first line. | `check`, `run`, `stage`, `tree` (null in the directory form), `attempt`, `argv`, `dir`, `timeout_s`, `started_at_ms` (the wall clock, Unix milliseconds). |
| `section` | A line `swarmops:section <title>` of the check's output. | `n`, from 1, and `title`. |
| `cut` | The log reached its bound. | `limit`. |
| `end` | The last line, once the output ended. | `outcome`, `exit_code` or `signal` when there is one, `duration_ms`, `sections` (each `n`, `title`, `start_ms`, `duration_ms`), `dropped_bytes`. |

A check opens a **section** with a line, on either stream, whose text starts with
`swarmops:section ` and whose rest, trimmed, is the title: that line is written as the runner's
`section` line, not as output. A section lasts until the next one opens or the output ends;
output before the first belongs to none. A title past 200 bytes is cut at a character's end with
`…`; past 1,000 sections a marker is an ordinary line; an empty title makes it an ordinary line.
`check run`'s answer gives each check's sections too.

**Bounds.** A log holds at most 64 MiB: past it one `cut` line is written and later output is
counted in the `end` line's `dropped_bytes`, not written; the outcome stands. A line keeps at most
16 KiB of text, the rest counted the same way. The answer's tails come from all of the output.
A reader reads a log as a stream, keeping only the lines it gives, and no further than 65 MiB
from its start, which no run writes past: a longer file, which a check writing to its own log's
path could leave, counts only the lines within that. A log that is not a regular file (a link,
which is not followed, a FIFO, a device) is `E_IO`.

The format is SwarmOps' own and provisional: it may change before logs are kept beside the store.

### The answer

`lines` are the last `--tail` lines (1 to 100,000; 1,000 by default), each with its number `n`,
its `at_ms`, its `stream` and its `text`; a line not in the format, which a check writing into its
own log could leave, is `other`, whole, with no time. `first_line` and `total_lines` say where they
are. `ended` says whether the log is complete, and `ended_by` how: `end`, its `end` line (with the
check's `outcome` and `duration_ms`), or the run's record of a check it never started (`not_run`,
no `log`); or `runner_gone`, the run's lock free with no `end` line, as a `check run` killed by
SIGKILL leaves it.

### The failed checks' logs

`check log --failed` answers for every check of the run's record whose outcome is not `passed`,
in run order: `failed`, `timed_out`, `not_started`, `cancelled` and, once the run has ended,
`not_run`. Each one's log is read as `check log NAME --run RUN --tail N` reads it, so a check
with no log answers no line and its outcome from the record.

| Field | Type | Present | Meaning |
| --- | --- | --- | --- |
| `failed` | array of strings | with `--failed` | Every check of the run that did not pass, in run order. |
| `more` | array of logs | with `--failed`, when more than one log is given | The logs of the failed checks after the first, in run order, each with the keys of the answer above. |
| `logs_omitted` | integer | when not 0 | The failed checks whose logs this answer leaves out; `failed` names them. |

The rest of the answer is the first failed check's log. **A run with one failed check answers
that log alone**: the envelope is what `check log NAME` gives, with `failed` naming it and no
`more`, and `--human` prints byte for byte what `check log NAME --human` prints. With several,
`--human` prints the logs one after another, a line `==> NAME <==` before each.

One answer gives at most 20 logs, and no more than keep `--tail` times their count within 100,000
lines: the first `min(failed, 20, max(1, 100,000 / tail))` in run order. With the default tail of
1,000, that is 20 logs; with `--tail 100000`, one. The others are read with a `check log NAME`
each.

A run where no check failed is `E_INVALID_ARG` with `details.reason: none_failed`, `details.run`
and `details.running`, whether the run's lock is held: of a run that still runs, a check not
started yet is not counted, so the answer is `none_failed` until a check of it has ended without
passing, and the hint says to wait or to follow one check. `--failed` goes with neither `CHECK`
nor `--follow`: the command line refuses both.

### `--follow`

`check log --follow` first prints the answer above, then, each time the check wrote whole lines
since its last look, one envelope with those lines (at most 1,000 an envelope), one envelope a
line of stdout, flushed at once, as `derive --follow` prints its envelopes. A look that finds
nothing prints nothing, and the next is `--interval-ms` later (200 by default). It ends with the
envelope that holds the log's end, `ended: true`, and exits 0; a follower started before its check
starts waits for the log to appear while the run runs. With `--human` it prints the log's lines as
the log holds them, each control character escaped (`\u{1b}` for ESC, `\t` for a tab), so that what
a check printed cannot drive the terminal; and nothing else. Output it cannot write ends it,
exit 1, with a line on stderr. A log is followed on its own host only.

### The MCP tool

`swarmops mcp` serves the workspace form as the tool `check_log` (`CheckLogReq`: `workspace`,
`check` or `failed`, `run`, `tail`), one answer per call: there is no `--follow`, since a tool
call answers once, no directory form and no `--run-lines`. Exactly one of `check` and
`failed: true` is given; neither or both is `E_INVALID_ARG` with `details.reason: check`. With
`failed` the answer is the one above, each log its last `tail` lines.
