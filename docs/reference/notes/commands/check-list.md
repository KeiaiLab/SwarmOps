`check list` says what a tree names as its checks. It reads one file, writes nothing, takes no
lock and needs no identity. Nothing in SwarmOps runs these checks yet: `ws land --check` still runs
`check.argv` of `.swarmops/workspaces.json` alone.

### Two forms

- **From the store**, `check list [REV]`: the tree of `REV` (default `main`), resolved once and
  recorded in `meta.resolved`; the result names that `tree` and says `vouched: false`.
- **From a directory**, `check list --dir DIR`: the files below `DIR` on this host, for a host with
  no store at hand, such as a CI job on a git checkout. It opens no store, so `--repo`,
  `SWARMOPS_REPO` and `--as` are not used, and the envelope has no `meta.repo`. The result, and an
  `E_CHECK_FILE` refusal's `details`, say `vouched: true`: only the caller stands behind what the
  directory holds, so no gate accepts such an answer. `.swarmops` and the file are never followed
  if they are symbolic links. `REV` and `--dir` together are a usage error.

In both forms a `.swarmops` that is a symbolic link is refused, `not_a_file`, and the message
names it: the store follows no link either, and such a tree is not listed as one without checks.

### The check file, version 1

`.swarmops/checks.toml` at the tree's root, in TOML 1.1 (which also reads every TOML 1.0 file):

```toml
version = 1

[check.unit]
argv = ["cargo", "test", "--locked"]

[check.lint]
argv = ["cargo", "clippy", "--", "-D", "warnings"]
dir = "crates/app"
timeout_s = 600
stages = ["before-land", "after-land"]
```

| Key | Required | Default | Rule |
| --- | --- | --- | --- |
| `version` | yes | | The integer `1`. |
| `[check.<name>]` | no | no checks | One table a check. The name is 1 to 64 bytes of `a-z`, `0-9`, `-` and `_`, starting with a letter or a digit. |
| `argv` | yes | | The program and its arguments, 1 to 1,024 strings, 64 KiB together, no NUL byte, the program not empty. It runs with no shell: steps and conditions belong in the repository's own scripts. |
| `dir` | no | `"."` | The directory of the tree it runs in: `.` or a repository path (no empty, `.` or `..` part, no leading or trailing `/`). Whether the tree holds it is not checked. |
| `timeout_s` | no | `900` | The most seconds it may run, 1 to 86,400. |
| `stages` | no | `["before-land"]` | When its answer is wanted: 1 to 6 of `before-land`, `after-land`, `scheduled`, `release`, `manual` and `deploy`, none twice. An unknown stage, an empty array, an item that is not a string, or a stage given twice is refused as `value`, at the line of the item (or the array) at fault. The key is `stages`, plural: `stage` is refused as an unknown key. |

The file holds at most 1 MiB and 1,000 checks. Any TOML table syntax reads alike: a header, dotted
keys (`check.unit.argv = [...]`) or an inline table. The [limits](../../limits.md) page lists each
bound with its constant.

**Any other key is refused**, at its line, as `unknown_key`. Later releases add keys to version 1
(what a check reads, when it runs, what it needs first); a build that does not know a key refuses
the file instead of listing, or one day running, a check without it. A change to what an
existing key means is a new version.

### The answer

The checks come sorted by name, so moving a table in the file changes nothing but their `line`.
Each has its `argv`, its `dir`, its `timeout_s` and its `stages` with the defaults filled in, the
`line` its table starts on, and `definition_digest`, a digest of `argv`, `dir` and `timeout_s`
(not of the name, nor of the stages, which say when an answer is wanted, not what the command
computes): two checks that differ only in name or stages, or a check renamed, share a digest, and
a default written out gives the same digest as one left out. `stages` come in the stages' own
order (`before-land`, `after-land`, `scheduled`, `release`, `manual`, `deploy`), whatever order
the file wrote them in. The same tree gives the same `result` byte for
byte, whichever changeset names it.

**The digest is provisional.** Its recipe is not part of the hash contract and may change in a
later release; compare digests made by one release only.

### A tree without the file

When the tree holds no `.swarmops/checks.toml`, `check.argv` of `.swarmops/workspaces.json`, if it
names a command, is the one check `default`: that argv, at the root, with `check.timeout_s` or 900,
the stage `before-land` (the check `ws land --check` runs before a land), and no `line`; `file` says
`.swarmops/workspaces.json`. That file is read in the shape and the size a workspace reads it in, at
most 64 KiB of JSON, so `check list` refuses every file that `ws land --check` refuses (as
`E_CHECK_FILE`, where the land says `E_INVALID_ARG`). It also holds `check.argv` and
`check.timeout_s` to version 1's rules, which the land does not: a timeout past 86,400 s, or an argv
whose program is empty, is refused here and accepted by the land. The land never reads
`check.timeout_s` at all, and takes its own `--check-timeout`; the `timeout_s` listed for `default`
is the one a later runner will use. A tree with neither file, or with an empty or missing
`check.argv`, lists no check and exits 0.

### One stage

`--stage STAGE` (the request's `stage`) lists only the checks whose `stages` hold `STAGE`, still
sorted by name, in either form, and the result echoes `stage`; without it every check is listed
and the result has no `stage`. A stage no check names lists none and exits 0. `--stage
before-land` lists what `check run` runs by default.

A tree that holds `.swarmops/checks.toml` is listed from it alone, whatever `workspaces.json`
says. `ws land --check` does not read the check file yet, so in such a tree the two can disagree
until checks run through SwarmOps.

### Refusals

A file that does not read is `E_CHECK_FILE`, exit 1, and nothing is listed. `details.reason` is
one of `too_large`, `not_text`, `not_a_file`, `syntax`, `version`, `unknown_key`, `name`, `value`
and `too_many`; `details.line` names the line (1 is the first) when the fault has one (keys
dotted past the reader's nesting depth are a `syntax` fault without one), and
`details.key` the key at fault as a dotted path. The faults are judged in file order after the
version, and the first one is the refusal. [Troubleshooting](../../../troubleshooting.md#e_check_file)
says what fixes each.

The directory form has two errors of its own. A `.swarmops` or a file in it that the host will not
let it read (a permission, say) is `E_IO`, exit 1, with `details.path` naming it below `DIR` and
`details.vouched: true`. A `DIR` that is not a directory is `E_INVALID_ARG`, exit 2, with
`details.reason: dir` and `details.dir` as given.

### Through MCP

The MCP tool is `check_list`, with an optional `rev`. It has the store form alone: the server
always serves the store it was started on, and a directory of the server's host is not something
an agent's request should name.
