`check plan` says, for one stage, which checks a change needs and why: each check `run` or
`not_applicable`, with one reason. It reads only: no identity, no lock, and nothing is written,
to a store or anywhere else.

**A plan informs.** It starts no check and skips none:
[`check run`](../../commands/check-run.md) runs every check of its stage, whatever a plan says.
What a plan is for today is the caller's own decision, such as whether a job that no gate
requires is worth starting.

### Four subjects

A plan is of exactly one subject; `subject` says which.

| Form | `subject` | The changed paths | The candidate's checks | Compared with |
| --- | --- | --- | --- | --- |
| `check plan REV [--target REF]` | `changeset` | What `REV` changed since its merge base with the target, from the store's diff | The check file of `REV`'s tree | The check file at the target's head |
| `check plan REV --landed [--base REV]` | `landed` | The paths [`land-check`](../../commands/land-check.md) gives for landing `REV` on the target | The check file the landed tree holds | The check file at the target's head |
| `check plan --ws NAME` or `--ws-path DIR` | `workspace` | The workspace's committed chain (its base to its head), then its edits on disk | The head's check file, or the workspace's own files when an edit changed it | The check file at the head of the workspace's target |
| `check plan --dir DIR [--changed PATH]… [--changed-from FILE]` | `paths` | The paths you give | The check file below `DIR` | Nothing: no target is at hand |

`--target` is `main` by default. `REV`, the target and `--base` are each resolved once, and
recorded in `meta.resolved`.

- **A changeset.** `tree` is `REV`'s root, `base` the merge base, `onto` the target's head. Two
  revisions whose first-parent chains share no changeset have no base: the changed paths are not
  known, and the plan is widened (`paths_unknown`).
- **A land.** `--landed` asks what [`land-check`](../../commands/land-check.md) asks, for the one
  change, and plans its answer. `clean`: `tree` is the root the landed changeset would have,
  which is neither `REV`'s nor the target's. `replayed` and `nothing_to_land`: no path changes,
  and no `tree` is named. A change that conflicts has no landed tree: `E_CONFLICT`, exit 3, with
  the report in `details.report` and the target's head in `details.onto`, and no plan. No
  changeset names the landed tree, so its check file is read where the item's paths say it comes
  from: at the target's head when the change did not touch it, at `REV` when the change added,
  modified or deleted it. When both sides changed it and it merged, the merged file is in no
  changeset: it is read at `REV` and the whole plan is widened (`check_file_changed`). `--base`
  is taken with `--landed` alone.
- **A workspace.** Named by `--ws` or `--ws-path` only: a plan never takes the workspace around
  the working directory. It is read without its lock, as `ws status` reads it, so it answers
  while another command holds the workspace. A workspace that is rebasing
  (`E_UNRESOLVED_CONFLICTS`), stale (`E_WS_STALE`), or whose files stand for another changeset
  than its head (`E_INVALID_ARG`, `details.reason: materialized`) is refused, as `check run`
  refuses it. Edits are not refused: they are what this form is for. `rev` is the workspace's
  head, `edits` counts its edits, and `tree` is the head's root when there is none and omitted
  when there is one, since no tree in the store is then what was planned. A sparse workspace
  that holds entries off its prefixes has edits in no list: the plan is widened
  (`paths_unknown`).
- **A directory.** It opens no store, so `--repo`, `SWARMOPS_REPO` and `--as` are not used and
  the envelope has no `meta.repo`. The answer, and an `E_CHECK_FILE` refusal's `details`, say
  `vouched: true`: only you stand behind the file and the paths, so no gate accepts such a plan.
  No path is looked up below `DIR`: a deleted file is a changed path like any other.

### One stage

A plan is of one stage: `--stage STAGE`, `before-land` by default, as `check run`. A check of two
stages is in both plans. `--check NAME`, repeatable, keeps only those checks of the plan; a name
the plan does not hold is `E_INVALID_ARG`, with `details.reason: stage` and the check's `stages`
when the file defines it for another stage, else `details.reason: check`.

### Verdicts and reasons

Each check has one reason: the first of these that holds, in this order.

| # | Holds when | `verdict` | `reason` | With |
| --- | --- | --- | --- | --- |
| 1 | Its definition is not the one the target's head has under its name | `run` | `definition_changed` | `definition`, and `was` for a rename |
| 2 | The whole plan is widened | `run` | `widened` | `widened`, the cause |
| 3 | It declares inputs, and a changed path is among them | `run` | `input_changed` | `path`, `pattern` |
| 4 | It declares no inputs, and at least one path changed | `run` | `no_inputs` | |
| 5 | Otherwise | `not_applicable` | `inputs_unchanged` | |

`not_applicable` has one way in, the last row: every changed path was read, each of them a
repository path, none is among the check's [inputs](../../commands/check-list.md#declared-inputs),
and its definition is the target's head's. `path` is the first changed path read that the check's
inputs match (path order from the store; your order in the directory form), and `pattern` the
first of its patterns, in declared order, that matches it.

### A definition against the target's head

Rule 1 compares the candidate's checks of the stage with those of the check file at the target's
head (`onto`), by `definition_digest`:

| `definition` | When |
| --- | --- |
| `modified` | Both have the name, and the digests differ |
| `renamed` | Only the candidate has the name, exactly one name that only the target has carries its digest, and no other name that only the candidate has does; `was` is that name |
| `added` | Only the candidate has the name, otherwise |
| `deleted` | Only the target has the name |

- A `deleted` check is listed although the candidate's file cannot run it, with the target's
  digest: a plan never gets shorter because a definition went away. A renamed check's old name is
  listed too, as `deleted`.
- A check's `inputs` are in its digest, so **a candidate that narrows its own inputs changed the
  definition, and the check runs**, whatever paths changed.
- `stages` is not in the digest: what is compared is membership of the planned stage. A check
  moved out of the stage is `deleted` for it, one moved in is `added`.
- A target with no check file makes every check `added`; a candidate with none makes every
  check of the target `deleted`. Neither side with a check of the stage is a plan of no check,
  exit 0.
- The directory form has no target, so it never says `definition_changed`: you vouch that the
  definitions are the target's.

### A widened plan

When the changed paths or the target's definitions are not known in full, the whole plan is
widened: every check whose definition did not change is `run`, `widened`, also one a changed
path already matched. `widened`, on the answer and on each such check, names the first cause
that holds, in this order:

| `widened` | When |
| --- | --- |
| `paths_unknown` | The candidate and the target share no history; a sparse workspace holds entries off its prefixes; the directory form was given neither `--changed` nor `--changed-from` |
| `paths_truncated` | More than 100,000 changed paths; a list past 16 MiB; a land-check item whose `paths` are cut at 1,000 |
| `path_unreadable` | A changed path given that is not a repository path |
| `check_file_changed` | The directory form: `.swarmops/checks.toml`, `.swarmops/workspaces.json` or `.swarmops` is among the changed paths, and no target is at hand to compare with. `--landed`: the check file merged |
| `target_unread` | The check file at the target's head does not read |

**An error is never a plan with fewer checks.** The candidate's check file not reading is
`E_CHECK_FILE`; a revision, a target or a workspace that names nothing, a list that does not
open, and a store that fails are their own errors, each with no `result`.

### The list of changed paths

The directory form takes its paths from `--changed PATH`, once for each path, and from
`--changed-from FILE`, a list; with both, the flags' paths are read first. `-` as `FILE` is
standard input.

- Entries end with a line feed, or with a NUL byte under `--nul`. A last entry without its end is
  taken; an empty entry is passed over.
- An entry is a changed path when it is a repository path from the directory's root: UTF-8, not
  absolute, no empty, `.` or `..` part, at most 4,096 bytes. Any other entry is
  `path_unreadable`. An empty `--changed` is unreadable too: `--changed ""` names no path, and
  is not a change of no path.
- In a list of lines, an entry that starts with `"` or holds a control character is unreadable
  too. A tool that writes one name a line quotes such names, and a quoted name read as a path
  would match nothing and plan its check `not_applicable`. Give such lists NUL-ended:
  `git diff --name-only -z BASE HEAD > changed.nul`, then `--changed-from changed.nul --nul`.
- At most 100,000 entries and 16 MiB are read; past either the plan is `paths_truncated`.
- **No `--changed` and no `--changed-from` is not a change of no path**: the paths are not
  known, and every check runs (`paths_unknown`). An empty list given, such as `--changed-from`
  an empty file, is a change of no path: every check is `not_applicable`.
- A `--changed-from` file that does not open or read is `E_IO`, with `details.path` as given and
  `details.vouched: true`: a mistyped path is an error, not a plan. The file is opened as you
  named it, a symbolic link followed.

### The answer

`vouched`, `subject` and `stage` come first; then, where the form has them, `rev`, `tree`,
`workspace`, `edits`, `target`, `onto`, `base` and `outcome` (of `--landed`: `clean`, `replayed`
or `nothing_to_land`); `file` and `version` as [`check list`](../../commands/check-list.md) gives
them; `changed`, the changed paths read (a path both committed and edited in a workspace is read
twice); `widened` when the whole plan is; and `checks`, sorted by name.

The same subject gives the same `result` byte for byte: it holds no time and no path of a host,
and two changesets of one tree on one target give the same `checks`.

Later releases add verdicts, reasons and causes
([versioning](../../../policies/versioning-and-stability.md)). A reader takes a verdict it does
not know as `run`. The exit status is 0 for every plan, whatever its verdicts.

### Through MCP

The MCP tool is `check_plan`: the changeset form, and with `landed` the land's. The directory
form is the command line's alone, since a directory of the server's host is not something an
agent's request should name. For a workspace's edits, call `ws_snapshot` first and plan the
changeset it returns.
