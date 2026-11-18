`check run` runs the checks of one stage that a tree's `.swarmops/checks.toml` names, one at a
time, and answers one envelope naming every check's outcome. It is how this repository's
maintainers and agents start the gate before a push: `so check run --dir . --stage before-land`.
The gate guides: the directory form is vouched for by the caller alone, and nothing records its
answer yet.

### Two forms

- **In a workspace**, `check run [NAME…]` with `--ws NAME`, `--ws-path DIR`, or the workspace
  around the working directory. The workspace must be clean: no edits on disk (`E_WS_DIRTY`, with
  the first 20 paths), not rebasing (`E_UNRESOLVED_CONFLICTS`), its base still on its target
  (`E_WS_STALE`). Its lock is taken (`E_WS_LOCKED` after a 5 s wait, retryable) and held until the
  run ends, so no `ws` command moves the files under a running check: a check that runs one gets
  `E_WS_LOCKED`. The check file is read from the store at the changeset the files stand for,
  never from the files on disk, and the result names that `changeset` and its root `tree`, the
  tree every check ran on; `vouched` is `false`. The store is only read: no writer lock is taken,
  and a check may write to the store itself. The runs are kept in the workspace's
  `.swarmws/check-runs/`.
- **In a directory**, `check run [NAME…] --dir DIR`: the checks of the files below `DIR` on this
  host, as they are, read as `check list --dir` reads them. It opens no store, so `--repo`,
  `SWARMOPS_REPO` and `--as` are not used, and the envelope has no `meta.repo`. The result says
  `vouched: true` and names no tree. The runs are kept below `SWARMOPS_CHECK_RUNS_DIR` (an
  absolute directory), else below the state directory's `swarmops/check-runs`
  (`$XDG_STATE_HOME`, `~/.local/state` or `~/Library/Application Support`), in a directory keyed
  by `DIR`'s canonical path, whose file `dir` names it. Nothing is written into `DIR`. Two runs of
  one directory at once are not kept apart.

### What runs

The checks whose `stages` hold `--stage` (default `before-land`), or of them only the `NAME`s
given, each run once, in name order. A name the file does not define is `E_INVALID_ARG` with
`details.reason: check`; a name not of the stage, `details.reason: stage` with the check's
`stages`; a stage with no check, or a tree with no check file and no `check.argv`, `no_checks`:
nothing runs then, so a gate that would run nothing fails. A file that does not read is
`E_CHECK_FILE`, as `check list` gives it.

Each check runs its `argv` with no shell and no stdin, in its `dir` below the root, in a process
group of its own, for at most its `timeout_s`; past it, the whole group is killed. However the
check ends, its group is killed once its output ends, or a second after it exits if a process it
left still holds the output: nothing it left running in its group outlives it. A check that
fails does not stop the run: every selected check runs. Outcomes:

| Outcome | When |
| --- | --- |
| `passed` | It exited 0. |
| `failed` | It exited with another code (`exit_code`), or a signal the runner did not send ended it (`signal`), or its log could not be written whole (a full disk, say): then `message` says why, whatever it exited with. |
| `timed_out` | It ran past `timeout_s`; its process group was killed. |
| `not_started` | Its `dir` is not a directory here (`not_started: dir`, as a sparse workspace may lack it), its program did not start (`not_started: program`, with the host's `message`), or its log or its own directory could not be made (`not_started: log`, with the host's `message`). |
| `cancelled` | A signal stopped the run while it ran; its process group was killed. |
| `not_run` | The run was stopped before it started: no process, no log. |

Only `passed` passes. Every check passed: the result, exit 0. Otherwise `E_CHECK_FAILED`, exit 1,
whose message names each check that did not pass and why, whose `details.run` is the whole result
below, and whose hint names `check log` for the first. A check that did not pass carries the last
8 KiB of each of its streams (`stdout_tail`, `stderr_tail`); one that passed does not.

### Signals

SIGINT, SIGTERM and SIGHUP stop the run: the running check's process group is killed, that check
is `cancelled`, the checks after it are `not_run`, and the answer is `E_CHECK_FAILED` with
`details.run.cancelled: true`. A signal once the last check has exited stops nothing but what
that check left in its group: the run ends as its checks did. A second signal changes nothing.
A process that left its check's group, and a check left running when `swarmops` itself is killed
with SIGKILL, are not stopped.

### The environment

A check of `.swarmops/checks.toml` gets these variables, and of the caller's environment only
`HOME`, `LANG`, `LC_ALL`, `LC_CTYPE`, `LOGNAME`, `PATH`, `TMPDIR`, `TZ` and `USER`, each when it is
set, and each `--pass NAME` that is set; nothing else. `inherited_env` lists the names given, never
their values.

| Variable | Value |
| --- | --- |
| `SWARMOPS_CHECK_NAME` | The check's name. |
| `SWARMOPS_CHECK_STAGE` | The stage run. |
| `SWARMOPS_CHECK_TREE` | The tree's 64 hex digits in a workspace; empty in the directory form. |
| `SWARMOPS_CHECK_ATTEMPT` | `1`. |
| `SWARMOPS_CHECK_RUN` | The run's id. |
| `SWARMOPS_CHECK_REPORT` | A path for the check's findings, not made; nothing reads it yet (M31-4). |
| `SWARMOPS_CHECK_OUTPUTS` | A path for the check's outputs, not made; nothing reads it yet (M31-6). |
| `SWARMOPS_CHECK_SUMMARY` | A path for the check's summary, not made; nothing reads it yet (M31-18). |
| `SWARMOPS_CHECK_ARTIFACTS` | A directory for the check's artifacts, made empty; nothing reads it yet (M36-3). |
| `SWARMOPS_CHECK_CHANGED` | Empty: no plan gives the changed paths yet (M30-4), so a check takes the whole tree as changed. |
| `SWARMOPS_WS` | The workspace's name, in the workspace form. |

A `--pass` name is 1 to 128 bytes of `[A-Za-z_][A-Za-z0-9_]*`, not a `SWARMOPS_CHECK_` name nor
`SWARMOPS_WS`, at most 64 of them; another is `E_INVALID_ARG` with `details.reason: pass`.
The one check `default` of `.swarmops/workspaces.json` keeps the caller's whole environment, as
`ws land --check` gives it, with these variables set over it; `inherited_all` says so.

### The run and its logs

A run's `dir` holds `run.json` (its `state`, `running` then `ended`, and the `result`), `lock`
(held while it runs), `logs/<check>.log` and `checks/<check>/`, the paths the environment names.
Each log holds both streams, a time on every line, and the sections a check opens; `check log`
reads one, and its page has the format. The newest 20 runs of a directory or a workspace are
kept, and a run whose lock is held is never removed. Nothing there is synced: the logs and
records are diagnostics, which nothing decides from. A log that cannot be written is its check's
outcome, and the run goes on; only a record that cannot be written makes the run `E_IO`, with the
outcomes so far in `details.run` and a hint naming the run's directory.

`duration_ms` is the run's, from its first check's start to its last's end, and each check's its
own; neither is a measured figure of any check.

### The MCP tool

`swarmops mcp` serves the workspace form as the tool `check_run`, whose arguments are
`CheckRunReq` (`workspace`, `stage`, `checks`): there is no directory form there, and no `--pass`,
since the server's environment is not the agent's to hand out; a check gets the fixed list of the
server's variables. A call holds the server until the run ends, each check bounded by its
timeout. SIGINT, SIGTERM or SIGHUP to the server stops the run as it stops `check run`: the call is
answered, and then the server exits. A check that did not pass makes the call's result an error,
`E_CHECK_FAILED`, with every outcome in `details.run`.
