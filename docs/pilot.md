# The pilot: SwarmOps on its own repository

The first real use of SwarmOps is on SwarmOps itself. Its git history goes into a store, one agent
session commits and lands a small real change through `swarmops mcp`, the store's history goes
back into a git branch, and this repository's own CI runs on that branch. The loop then runs a
second time, with a commit from a person on the trunk and four agent sessions landing at once, one
of them killed on the way. Nothing is at risk: the repository on GitHub stays the source of truth
until the pilot passes, the pilot pushes nothing but one `feat/` branch, and removing the pilot's
directory and that branch undoes all of it.

[operations.md](operations.md) explains each command. This runbook says what to run, in order,
what to measure, what counts as a pass, and how to roll back. Every command here was run in a dry
run on 2026-10-20 with the binary built from this repository, and the outputs below are copied
from it (`…` marks a cut, and the pilot directory's path is left out), except what needs a person
or write access to GitHub: the interactive Claude Code session of step 3, and the push of step 6
and its deletion in the rollback, which the dry run made to a local stand-in for the remote. Each
is marked where it comes. Steps 7 to 9 ran later the same day, and say so.

Three roles take part. The **operator** runs the steps and writes the report; the **agent** is one
Claude Code session, or the scripted client of the [appendix](#appendix-the-scripted-session);
the **maintainer** reads the report and decides whether SwarmOps goes on to a service repository.

## What you need

- **The binary under trial.** A release archive checked against its `SHA256SUMS`
  ([operations.md](operations.md#from-a-release)), or a build of the commit under trial: in a clone
  at that commit, `cargo build --release --locked -p swarmops-cli`, never with
  `--features test-hooks`. It must hold `git import`, `land` and `git export`.
- **GitHub access.** `gh` logged in with read access to `KeiaiLab/SwarmOps` for the mirror, and
  push access for the CI run of step 6.
- **The repository's gate.** rustup, a C compiler, and the tools CI installs: `just` 1.58.0,
  `cargo-nextest` 0.9.146 and `cargo-deny` 0.20.2.
- **Claude Code** for the agent session, or `python3` for the scripted one.
- **A directory on a local, native Linux filesystem** (ext4 or XFS; not NFS, not under `/mnt/`)
  with about 4 GB free. On `stable` at 470 commits (a run on 2026-10-20, later than the dry run),
  the pilot's directory took 3.9 GB, the gate's build output 3.3 GB of it, and the store 57 MB
  after the import and 92 MB after the exports. With `CARGO_PROFILE_DEV_DEBUG=line-tables-only` in
  step 6's environment the build output is smaller (line tables and no other debug info; the same
  tests run): 2.2 GB in the dry run, whose store held SwarmOps' 422 commits in 51 MB.

Write the report as you go: each step's command, exit code, wall time, the load average before
and after (`uptime`), and the figures [Measure](#measure) lists.

## Step 0: Set up

`PILOT` names the pilot's directory; every path below is under it. Put the binary under trial in
`$PILOT/bin`, and put that first on `PATH`, so that every step runs it and no other.

```sh
export PILOT=/path/to/local-disk/swarmops-pilot    # a new directory: the placeholder is yours to choose
mkdir -p "$PILOT/bin"
install -m 0755 target/release/swarmops "$PILOT/bin/"    # from the build's clone; or the archive's swarmops
export PATH="$PILOT/bin:$PATH"
swarmops --version
sha256sum "$PILOT/bin/swarmops"
```

```text
swarmops 0.1.0
0dd66074107bd766e0054bf0b6b2415f8df457590cef9586b23afdb66c6a1e46  …/bin/swarmops
```

Record the version, the SHA-256 and, for a build, the 40-hex commit it was built from
(`git rev-parse HEAD` in its clone).

## Step 1: Mirror the repository

The mirror is the only copy of the repository the pilot reads, and no step writes to it. `START`,
the commit `stable` holds when the pilot starts, is the baseline of the round trip and of the
rollback.

```sh
gh repo clone KeiaiLab/SwarmOps "$PILOT/source.git" -- --mirror
START=$(git -C "$PILOT/source.git" rev-parse stable)
git -C "$PILOT/source.git" rev-list --count "$START"
```

```text
Cloning into bare repository '…/source.git'...
422
```

## Step 2: Import the history

```sh
export SWARMOPS_REPO="$PILOT/store"
swarmops init "$SWARMOPS_REPO" --name swarmops
swarmops --as pilot/operator git import "$PILOT/source.git" --ref stable --into main
swarmops verify --deep
du -sh "$SWARMOPS_REPO"
```

```text
{"ok":true,"op":"repo_init","api":1,"result":{"repo_id":"83fdd03d04ec833767f2dba3fddde6ce","name":"swarmops","store_version":1,"changesets":0,"refs":0},"meta":{"repo":"…","elapsed_ms":244,"resolved":{},"auth":"trust"}}
{"ok":true,"op":"git_import","api":1,"result":{"imported":422,"skipped":0,"head_changeset":"a6bcf9d8932d205f14023823bd8aadd8e3edc2b7c357bee5da57de7e227e8019","git_head_sha":"92191bd495fae7496c9b6a64ab2891f63fbfc4f4","target_ref":"main","ref_moved":true,"lossy":[],"lossy_total":0},"meta":{"repo":"…","elapsed_ms":3811,"resolved":{},"auth":"trust"}}
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":7647,"changesets_checked":422,"problems":[],"findings":[]},"meta":{"repo":"…","elapsed_ms":225,"resolved":{},"auth":"trust"}}
51M	…/store
```

`imported` must equal step 1's count, with `lossy_total` 0: the import takes no `--lossy` class
here, so a commit SwarmOps could not hold exactly would have failed the import instead.
`skipped` is 0 in a new store: it counts the commits the store held already that the import's
walk met at its frontier, where it stopped, not the history the store holds.

## Step 3: One agent session

Pick the change before the session starts, and write it in the report: one small change the
maintainers want anyway, touching one to three files, that `just check` covers. A documentation
fix from the open issues is the right size. The dry run changed one line of
`docs/known-limitations.md`, whose first paragraph still said "as of M1.6" after M2 had landed.
Such a line moves on with the landings, so the steps below make no edit of their own: they take
the change you picked.

The agent reaches the store through `swarmops mcp` alone, with its own identity, `pilot/agent-1`:

```sh
mkdir -p "$PILOT/agent"
cat > "$PILOT/agent/.mcp.json" <<EOF
{
  "mcpServers": {
    "swarmops": {
      "command": "$PILOT/bin/swarmops",
      "args": ["mcp", "--repo", "$PILOT/store"],
      "env": {"SWARMOPS_AGENT": "pilot/agent-1"}
    }
  }
}
EOF
```

**With Claude Code** (not run in the dry run, which used the scripted client below),
start a session in `$PILOT/agent`, approve the project's `swarmops` server when Claude Code asks,
and give it this prompt with the change filled in:

```sh
cd "$PILOT/agent" && claude
```

```text
Use only the swarmops tools. Read <path> at main: ref_list gives main's full changeset id, and
read_file reads at that id (max_bytes 32768 at most; read on from next_offset while truncated).
Make this change: <the change>. Commit the whole new file with parent set to the id you read.
Then land the changeset with a new idempotency_key. Reply with the landed changeset id and
every error a tool returned, with its code.
```

Claude Code keeps the session's transcript as JSON lines under `~/.claude/projects/`. In it, the
change must go through the tools `mcp__swarmops__commit` and `mcp__swarmops__land`, and each tool
result is an envelope whose `meta.elapsed_ms` is the server's own time for that call.

**With the scripted client** (the dry run's way, and the way for an operator that is itself an
agent): write the changed file's new content to a file, then run the
[appendix](#appendix-the-scripted-session)'s `mcp_session.py`, which drives the same server through
the same `.mcp.json`: it reads the file at main's head, commits the new content on that head, lands
it, shows the result, and prints one line per tool call. The export adds its own trailers to
the commit message.

Save the appendix's script as `$PILOT/mcp_session.py`. It changes one file: `FILE` names it by
its path in the repository, and `SUBJECT` is the change in one line. Set them for your change,
and start the new content from the file as `$START` holds it:

```sh
FILE=docs/known-limitations.md    # the file your change touches: the example is yours to replace
SUBJECT=                          # your change in one line, the commit's subject: write it here
git -C "$PILOT/source.git" show "$START:$FILE" > "$PILOT/new-content"
```

Make the change in `$PILOT/new-content`, with an editor or with the agent's own edit, then run
the session; its first line stops it while `SUBJECT` is empty:

```sh
: "${SUBJECT:?write your change in one line on the SUBJECT line above}" &&
python3 -I "$PILOT/mcp_session.py" "$PILOT/agent/.mcp.json" "$FILE" "$PILOT/new-content" \
  "$SUBJECT"
```

Before the commit, the script compares the new content with the whole file it read at main's
head. When the two are the same, as when the change was not made, it stops with a message that
says so and exits 1, and nothing is recorded: make the change, and run it again.

The dry run's lines, for its change and the subject `docs: known limitations as of M2`; its file
fit in one `read_file` page:

```text
{"tools": 18}
{"tool": "ref_list", "wall_ms": 0, "elapsed_ms": 0, "ok": true, "code": null}
{"tool": "read_file", "wall_ms": 1, "elapsed_ms": 0, "ok": true, "code": null}
{"tool": "commit", "wall_ms": 50, "elapsed_ms": 49, "ok": true, "code": null}
{"tool": "land", "wall_ms": 65, "elapsed_ms": 64, "ok": true, "code": null}
{"tool": "show", "wall_ms": 2, "elapsed_ms": 0, "ok": true, "code": null}
{"base": "a6bcf9d8932d205f14023823bd8aadd8e3edc2b7c357bee5da57de7e227e8019", "changeset": "b0e4812ba53c76c6e3cd8541b2f5e276cfaef7efcae23420f169b6d574ffbfeb", "landed": "41b15c96e8bf50b1ecf88faf6d0f7718589220b8be2421531c8c745ff997857b", "status": "landed", "attempts": 1}
```

The block as written here ran on 2026-10-20 too, against `stable` at `23ce4a1`, whose
`docs/known-limitations.md` takes two pages, and so two `read_file` lines.

## Step 4: Check the store

```sh
swarmops verify --deep
swarmops show main --human
swarmops op log -n 2 --human
```

```text
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":7652,"changesets_checked":424,"problems":[],"findings":[]},"meta":{"repo":"…","elapsed_ms":290,"resolved":{},"auth":"trust"}}
id: 41b15c96e8bf50b1ecf88faf6d0f7718589220b8be2421531c8c745ff997857b
seq: 423
parents[0]: a6bcf9d8932d205f14023823bd8aadd8e3edc2b7c357bee5da57de7e227e8019
root: 836aa81db9f17fbdee0971434bfa2d6f408f15ee2aa1fa5ddefafd6c811caece
author.agent: pilot/agent-1
lander.agent: pilot/agent-1
…
ops[0].op: 3
ops[0].kind: land
ops[0].changesets[0].id: 41b15c96e8bf50b1ecf88faf6d0f7718589220b8be2421531c8c745ff997857b
ops[0].changesets[0].agent: pilot/agent-1
…
ops[0].ref_moves[0].name: main
ops[0].ref_moves[0].old: a6bcf9d8932d205f14023823bd8aadd8e3edc2b7c357bee5da57de7e227e8019
ops[0].ref_moves[0].new: 41b15c96e8bf50b1ecf88faf6d0f7718589220b8be2421531c8c745ff997857b
ops[0].ref_moves[0].actor: pilot/agent-1
…
ops[1].op: 2
ops[1].kind: commit
ops[1].changesets[0].id: b0e4812ba53c76c6e3cd8541b2f5e276cfaef7efcae23420f169b6d574ffbfeb
ops[1].changesets[0].agent: pilot/agent-1
…
```

`verify --deep` must find nothing. `show` names who made and who landed the change: both must be
the session's identity, `author.agent: pilot/agent-1` and `lander.agent: pilot/agent-1`. `op log`
lists the session's commit and its land as the two newest operations, the land's move of `main`
made by `pilot/agent-1`.

## Step 5: Export, and check the round trip

Export into a new bare repository whose branch is called `stable`, as the trunk is:

```sh
git init --bare -q -b stable "$PILOT/export.git"
swarmops git export "$PILOT/export.git" --ref main --branch stable
```

```text
{"ok":true,"op":"git_export","api":1,"result":{"exported":423,"objects_written":7651,"head_changeset":"41b15c96e8bf50b1ecf88faf6d0f7718589220b8be2421531c8c745ff997857b","head_sha":"c9735870db2943e6d5ae2bd824e61d764fffbfbc","branch":"stable","branch_moved":true,"incremental":false,"rewritten":0},"meta":{"repo":"…","elapsed_ms":6143,…}}
```

Every commit of the original history must come back under its own id, with the session's change
on top as one new commit, and git must accept the repository:

```sh
git -C "$PILOT/export.git" merge-base --is-ancestor "$START" stable && echo "the start is in the export"
git -C "$PILOT/export.git" rev-list --count "$START"..stable
git -C "$PILOT/export.git" log --format='%h %an <%ae> %s' "$START"..stable
git -C "$PILOT/export.git" diff --stat "$START" stable
git -C "$PILOT/export.git" fsck --strict
git -C "$PILOT/export.git" log -1 --format=%B stable
```

```text
the start is in the export
1
c973587 pilot/agent-1 <agent-1@pilot.swarmops.local> docs: known limitations as of M2
 docs/known-limitations.md | 2 +-
 1 file changed, 1 insertion(+), 1 deletion(-)
docs: known limitations as of M2

SwarmOps-Changeset: 41b15c96e8bf50b1ecf88faf6d0f7718589220b8be2421531c8c745ff997857b
SwarmOps-Land-Key: 2660fdfe-6f2d-4f01-8ad8-ba613df59534
SwarmOps-Squashed-From: b0e4812ba53c76c6e3cd8541b2f5e276cfaef7efcae23420f169b6d574ffbfeb
```

A commit's id covers its parents' ids, so `$START` in the export under its own id is the whole
original history under the same ids, and `fsck` checks that every object it reaches is there and
sound (it prints nothing, and exits 0). The count is the new commits: one. The same store must give
the same commits every time: a second export, into another new repository, must give the same head.

```sh
git init --bare -q -b stable "$PILOT/export2.git"
swarmops git export "$PILOT/export2.git" --ref main --branch stable
git -C "$PILOT/export.git" rev-parse stable
git -C "$PILOT/export2.git" rev-parse stable
```

```text
{"ok":true,"op":"git_export","api":1,"result":{"exported":423,"objects_written":7651,"head_changeset":"41b15c96e8bf50b1ecf88faf6d0f7718589220b8be2421531c8c745ff997857b","head_sha":"c9735870db2943e6d5ae2bd824e61d764fffbfbc",…,"rewritten":0},"meta":{"repo":"…","elapsed_ms":5137,…}}
c9735870db2943e6d5ae2bd824e61d764fffbfbc
c9735870db2943e6d5ae2bd824e61d764fffbfbc
```

## Step 6: Run the repository's CI on the export

First the gate, locally, on a checkout of the exported head; it is the recipe CI runs:

```sh
git clone -q -b stable "$PILOT/export.git" "$PILOT/export-tree"
cd "$PILOT/export-tree" && just check
```

The dry run's gate passed (exit 0, 187 s from a cold build); its summary lines:

```text
     Summary [  80.974s] 661 tests run: 661 passed, 15 skipped
     Summary [   0.570s] 1 test run: 1 passed, 0 skipped
advisories ok, bans ok, licenses ok, sources ok
```

Then CI itself, which runs on every push to a `feat/` branch. This pushes the exported head to a
new branch of the GitHub repository, and nothing else (not run in the dry run, which pushed the
same refspec to a local mirror standing in for the remote):

```sh
BRANCH=feat/pilot-$(date +%F)
git -C "$PILOT/export.git" push git@github.com:KeiaiLab/SwarmOps.git "stable:refs/heads/$BRANCH"
gh run list -R KeiaiLab/SwarmOps --workflow ci.yml --commit "$(git -C "$PILOT/export.git" rev-parse stable)"
```

The dry run's push, to its stand-in:

```text
To …
 * [new branch]      stable -> feat/pilot-2026-10-20
```

The run shows up about a minute after the push. Wait for it, then read the one check that counts,
`tests-pass` (the run's database id is the number `gh run list` prints):

```sh
gh run watch <run id> -R KeiaiLab/SwarmOps --exit-status
gh run view <run id> -R KeiaiLab/SwarmOps --json jobs --jq '.jobs[] | select(.name == "tests-pass") | .conclusion'
```

The source's own result, to compare with, is the `tests-pass` of the run CI made for `$START` on
`stable`. The dry run read it with the same commands:

```sh
gh run list -R KeiaiLab/SwarmOps --workflow ci.yml --commit "$START"
```

```text
completed	success	merge: stable into feat/m2-diff-history again, before the landing (st…	CI	stable	push	39363962997	4m11s	2026-10-20T03:22:40Z
completed	success	merge: stable into feat/m2-diff-history again, before the landing (st…	CI	feat/m2-diff-history	push	39363567547	4m1s	2026-10-20T03:18:09Z
```

```sh
gh run watch 39363962997 -R KeiaiLab/SwarmOps --exit-status
gh run view 39363962997 -R KeiaiLab/SwarmOps --json jobs --jq '.jobs[] | select(.name == "tests-pass") | .conclusion'
```

```text
Run CI (39363962997) has already completed with 'success'
success
```

## Step 7: A commit on the trunk, and the second import

The loop runs a second time. While the agents are away, a person pushes a commit to the trunk, and
the next import takes it in. The trunk is `export.git`'s `stable`, which stands in for the remote's
(the pilot pushes nothing to GitHub's): clone it, commit a change as yourself on top of the
exported head, push it back, and import. The outputs of steps 7 to 9 are from a later run on
2026-10-20, against `stable` at 522 commits.

```sh
git clone -q -b stable "$PILOT/export.git" "$PILOT/person-tree"
sed -i '1s/$/ (edited by a person)/' "$PILOT/person-tree/CODE_OF_CONDUCT.md"
git -C "$PILOT/person-tree" -c user.name='Pat Person' -c user.email=pat@example.com \
  commit -q -a -m 'docs: a person edits the trunk'
git -C "$PILOT/person-tree" push -q origin stable
PERSON=$(git -C "$PILOT/person-tree" rev-parse HEAD)
swarmops --as pilot/operator git import "$PILOT/export.git" --ref stable --into main
swarmops verify --deep
```

```text
{"ok":true,"op":"git_import","api":1,"result":{"imported":1,"skipped":1,"head_changeset":"cfe2cf8a…","git_head_sha":"8d65c403…","target_ref":"main","ref_moved":true,"old_head":"ec7db4b8…","lossy":[],"lossy_total":0},"meta":{"repo":"…","elapsed_ms":135,"resolved":{},"auth":"trust"}}
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":9074,"changesets_checked":525,"problems":[],"findings":[]},"meta":{"repo":"…","elapsed_ms":281,"resolved":{},"auth":"trust"}}
```

`imported` is 1, the person's commit, and `skipped` is 1, the exported head where the import's walk
stopped. `main` moved to the person's commit (`ref_moved`, with `old_head` the head it left) and
holds what the trunk holds again. Had an agent landed after the export, both sides would have
moved, and the import would have failed with `E_GIT_IMPORT_DIVERGED`
([operations.md](operations.md), "One writer per trunk"); step 3's agent landed before the export.

## Step 8: Several sessions at once

One session at a time is the easy case. A swarm lands changes at the same moment, and an agent can
die at any point. This step starts four scripted sessions together, each an agent of its own,
`pilot/agent-2` to `pilot/agent-5`, and each puts its mark, `[agent-N]`, at the end of the first
line of a file:

- Agents 2 and 3 mark the same file, `CONTRIBUTING.md`, so the same line. Whichever lands second
  meets `E_CONFLICT` and lands again with a resolution that keeps both marks.
- Agent 4 marks `SECURITY.md`. Nothing conflicts, but its land may find `main` moved and merge
  again.
- Agent 5 marks `RELEASE.md`. It is killed with `kill -9` as soon as its commit is done, while its
  land is under way, and runs again.

The marks stand for any edits of that shape; for another repository, name three of its files. The
four sessions start from the same changeset, `BASE`, the head step 7 made, as agents that read
`main` at the same moment do, and take their file's content from the trunk of the git side, which
holds what `main` holds. They land here, after step 7's import and before the export, because
between an import and the next export only one side may change the trunk. A session's key is a uuid
v7 (RFC 9562), which `uuidgen` does not make by default; the function below does. The commit and
the land both take it as their idempotency key, so the same command run again, with the same key
and `BASE`, makes the same commit and lands it once.

```sh
field() { python3 -I -c 'import functools, json, sys; print(functools.reduce(lambda v, k: v[k], sys.argv[1].split("."), json.load(sys.stdin)))' "$1"; }
uuid7() { python3 -I -c 'import os, time, uuid; b = bytearray((time.time_ns() // 1_000_000).to_bytes(6, "big") + os.urandom(10)); b[6] = b[6] & 15 | 112; b[8] = b[8] & 63 | 128; print(uuid.UUID(bytes=bytes(b)))'; }
BASE=$(swarmops resolve main | field result.id)
for n in 2 3 4 5; do
  mkdir -p "$PILOT/agent-$n"
  cat > "$PILOT/agent-$n/.mcp.json" <<EOF
{"mcpServers": {"swarmops": {"command": "$PILOT/bin/swarmops", "args": ["mcp", "--repo", "$PILOT/store"],
  "env": {"SWARMOPS_AGENT": "pilot/agent-$n"}}}}
EOF
done
mark() { git -C "$PILOT/export.git" show "stable:$2" | sed "1s/\$/ [agent-$1]/" > "$PILOT/new-$1"; }
mark 2 CONTRIBUTING.md; mark 3 CONTRIBUTING.md; mark 4 SECURITY.md; mark 5 RELEASE.md
git -C "$PILOT/export.git" show stable:CONTRIBUTING.md | sed '1s/$/ [agent-2] [agent-3]/' > "$PILOT/merged"
cat > "$PILOT/resolve.json" <<EOF
{"files": {"CONTRIBUTING.md": {"content": {"file": "$PILOT/merged"}}}}
EOF
```

`resolve.json` is what `land --resolve` takes ([operations.md](operations.md), the daily loop's
step 2) for the shared file: the file with both marks, which the server reads from its path. A
session adds the one thing it cannot know beforehand, `against`, the head of `main` that the
conflict report names.

```sh
session() {  # session N FILE KEY [option ...]: one scripted session of agent-N; its last line is the exit code
  n=$1 file=$2 key=$3; shift 3
  python3 -I "$PILOT/mcp_session.py" "$PILOT/agent-$n/.mcp.json" "$file" "$PILOT/new-$n" \
    "docs: agent-$n marks $file" \
    --key "$key" --parent "$BASE" "$@"
  echo "exit $?"
}
K2=$(uuid7); K3=$(uuid7); K4=$(uuid7); K5=$(uuid7)
set -m    # a background job gets a process group of its own, so that one kill takes a whole session
session 2 CONTRIBUTING.md "$K2" --resolve "$PILOT/resolve.json" > "$PILOT/session-2.out" 2>&1 &
session 3 CONTRIBUTING.md "$K3" --resolve "$PILOT/resolve.json" > "$PILOT/session-3.out" 2>&1 &
session 4 SECURITY.md "$K4" > "$PILOT/session-4.out" 2>&1 &
session 5 RELEASE.md "$K5" > "$PILOT/session-5.out" 2>&1 &
PID5=$!
for i in $(seq 5000); do grep -q '"tool": "commit"' "$PILOT/session-5.out" && break; sleep 0.002; done
kill -9 -- "-$PID5"    # agent 5's commit is done: its land is under way
wait; set +m
for n in 2 3 4 5; do echo "== agent-$n"; cat "$PILOT/session-$n.out"; done
```

Each session prints one line per tool call, and its last line is its exit code: 0 when it landed.
One run's output:

```text
== agent-2
{"tools": 18}
{"tool": "read_file", "wall_ms": 1, "elapsed_ms": 0, "ok": true, "code": null}
{"tool": "commit", "wall_ms": 35, "elapsed_ms": 34, "ok": true, "code": null}
{"tool": "land", "wall_ms": 21, "elapsed_ms": 20, "ok": true, "code": null}
{"tool": "show", "wall_ms": 0, "elapsed_ms": 0, "ok": true, "code": null}
{"base": "cfe2cf8a…", "changeset": "5a69038c…", "landed": "b49bc915…", "status": "landed", "attempts": 1}
exit 0
== agent-3
{"tools": 18}
{"tool": "read_file", "wall_ms": 1, "elapsed_ms": 0, "ok": true, "code": null}
{"tool": "commit", "wall_ms": 46, "elapsed_ms": 45, "ok": true, "code": null}
{"tool": "land", "wall_ms": 1, "elapsed_ms": 0, "ok": false, "code": "E_CONFLICT"}
{"conflicts": ["CONTRIBUTING.md"], "against": "b49bc915…"}
{"tool": "land", "wall_ms": 34, "elapsed_ms": 34, "ok": true, "code": null}
{"tool": "show", "wall_ms": 0, "elapsed_ms": 0, "ok": true, "code": null}
{"base": "cfe2cf8a…", "changeset": "9253105c…", "landed": "12fb489f…", "status": "landed", "attempts": 1}
exit 0
== agent-4
{"tools": 18}
{"tool": "read_file", "wall_ms": 1, "elapsed_ms": 0, "ok": true, "code": null}
{"tool": "commit", "wall_ms": 32, "elapsed_ms": 31, "ok": true, "code": null}
{"tool": "land", "wall_ms": 66, "elapsed_ms": 65, "ok": true, "code": null}
{"tool": "show", "wall_ms": 0, "elapsed_ms": 0, "ok": true, "code": null}
{"base": "cfe2cf8a…", "changeset": "80497bcb…", "landed": "f69ce123…", "status": "landed", "attempts": 3}
exit 0
== agent-5
{"tools": 18}
{"tool": "read_file", "wall_ms": 1, "elapsed_ms": 0, "ok": true, "code": null}
{"tool": "commit", "wall_ms": 59, "elapsed_ms": 58, "ok": true, "code": null}
```

Agent 2 landed first, in one attempt. Agent 3 met `E_CONFLICT` at its first land, which found the
conflict at once and recorded nothing: the `conflicts` line names the file and `against`, the head
of `main` that the report names, and the land after it carried `resolve.json` and went through.
Agent 4 needed three attempts: `main` moved twice while its land was merging, and each time the land
merged again onto the new head and tried to publish again. Agent 5's output ends at its `commit`
line: the session was killed there and has no exit code. (The shell reports the kill; the file does
not.)

Which of the two sessions on the shared line meets the conflict, and which lands need more than one
attempt, differs from run to run. In a loop that ran this step 210 times, each time on a copy of
the store from step 7 and with agent 5 killed at a varied point (46 times before its `commit`
line), one of the two met the conflict every time, 134 of the 630 sessions that were not killed
needed more than one attempt, and every run ended with each key on `main` once and `verify --deep`
clean.

Run agent 5's session again, with the same key and `BASE`, and once more after it:

```sh
session 5 RELEASE.md "$K5"
session 5 RELEASE.md "$K5"
```

```text
{"tools": 18}
{"tool": "read_file", "wall_ms": 1, "elapsed_ms": 0, "ok": true, "code": null}
{"tool": "commit", "wall_ms": 5, "elapsed_ms": 4, "ok": true, "code": null, "replayed": true}
{"tool": "land", "wall_ms": 24, "elapsed_ms": 23, "ok": true, "code": null}
{"tool": "show", "wall_ms": 0, "elapsed_ms": 0, "ok": true, "code": null}
{"base": "cfe2cf8a…", "changeset": "56b98c63…", "landed": "431d2fe8…", "status": "landed", "attempts": 1}
exit 0
{"tools": 18}
{"tool": "read_file", "wall_ms": 1, "elapsed_ms": 0, "ok": true, "code": null}
{"tool": "commit", "wall_ms": 11, "elapsed_ms": 11, "ok": true, "code": null, "replayed": true}
{"tool": "land", "wall_ms": 1, "elapsed_ms": 0, "ok": true, "code": null}
{"tool": "show", "wall_ms": 0, "elapsed_ms": 0, "ok": true, "code": null}
{"base": "cfe2cf8a…", "changeset": "56b98c63…", "landed": "431d2fe8…", "status": "replayed", "attempts": 1}
exit 0
```

The first command is agent 5's session run again. Its `commit` line says `replayed`: the store held
that commit already, and nothing was written for it. Here its land says `landed`: the first run was
killed before it recorded the land. The second command runs the session a third time, on purpose:
the key has landed, so the land answers `replayed` and writes nothing.

A land is recorded by one journal frame (the storage spec, §2 rule 3 and §3). A
session killed before its frame is complete has recorded nothing of the land, and any objects it
wrote stay in the store, unreferenced: nothing collects them yet ([operations.md](operations.md),
"Limits"). A session killed after it leaves a land that stands: the next write completes what the
dead writer left, and a `commit`, ref move, undo, import or export that did so says it in
`meta.recovered`, where a `land` reports nothing of it. Neither leaves the store locked, since the
lock is the kernel's and goes with the process. Either way the second run is right: `landed` where
the first recorded nothing, `replayed` where it did. If `kill` finds no process, the session had
finished, and the first command answers `replayed`.

In the loop, the second run answered `landed` 173 times and `replayed` 37 times, and the third run
`replayed` every time. None of the lines above carries `recovered`; two lines of the loop did. One
was a `commit` line with `"recovered": {"frames_listed": 1, …, "records_replayed": 1, …}`, a write
that followed the kill listing a frame that the dead session had left complete. Such a line is no
failure; the report has it.

Now check the store, count each key on `main`, and read the shared line:

```sh
swarmops verify --deep
python3 -I - "$BASE" "$K2" "$K3" "$K4" "$K5" <<'PY'
import json, subprocess, sys

def show(rev):
    out = subprocess.run(["swarmops", "show", rev], capture_output=True, text=True).stdout
    return json.loads(out)["result"]

base, keys = sys.argv[1], sys.argv[2:]
landed, rev = [], "main"
while rev != base:  # main's first parents, newest first, down to where the sessions started
    changeset = show(rev)
    landed.append(changeset["land_key"])
    rev = changeset["parents"][0]
for key in keys:
    print(key, landed.count(key))
print(len(landed), "changesets landed since BASE,", len(set(landed)), "keys")
PY
swarmops cat main:CONTRIBUTING.md --lines 1-1 | field result.content
```

```text
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":9097,"changesets_checked":533,"problems":[],"findings":[]},"meta":{"repo":"…","elapsed_ms":255,"resolved":{},"auth":"trust"}}
01a15867-df88-7454-a589-779509174369 1
01a15867-dfc9-7e4a-935f-c17a09f61d52 1
01a15867-e00a-735a-9fba-a197f6344317 1
01a15867-e04b-703f-af6d-29a99294878c 1
4 changesets landed since BASE, 4 keys
# Contributing to SwarmOps [agent-2] [agent-3]
```

`verify --deep` finds nothing, after a session killed during its land. Each key is on `main` once;
four changesets landed since `BASE`, under four keys; and the shared line holds both marks, so the
land that met the conflict kept the other agent's change.

## Step 9: The second export

The export writes the commits the branch lacks, the four sessions', on top of the person's, which
the git side holds already:

```sh
swarmops git export "$PILOT/export.git" --ref main --branch stable
git -C "$PILOT/export.git" merge-base --is-ancestor "$PERSON" stable && echo "the person's commit is in the export"
git -C "$PILOT/export.git" rev-list --count "$PERSON"..stable
git -C "$PILOT/export.git" log --format='%h %an <%ae> %s' "$PERSON"..stable
git -C "$PILOT/export.git" fsck --strict
```

```text
{"ok":true,"op":"git_export","api":1,"result":{"exported":4,"objects_written":12,"head_changeset":"431d2fe8…","head_sha":"df80904e…","branch":"stable","old_sha":"8d65c403…","branch_moved":true,"incremental":true,"rewritten":0},"meta":{"repo":"…","elapsed_ms":28,…}}
the person's commit is in the export
4
df80904 pilot/agent-5 <agent-5@pilot.swarmops.local> docs: agent-5 marks RELEASE.md
c802a4d pilot/agent-4 <agent-4@pilot.swarmops.local> docs: agent-4 marks SECURITY.md
949ec12 pilot/agent-3 <agent-3@pilot.swarmops.local> docs: agent-3 marks CONTRIBUTING.md
74b1fe1 pilot/agent-2 <agent-2@pilot.swarmops.local> docs: agent-2 marks CONTRIBUTING.md
```

The export is incremental: it moved `stable` from `old_sha`, the person's commit, by four commits
and twelve objects, and rewrote nothing. The person's commit is in the history under the id the
person pushed it with, since a commit that came from git goes back as itself; the four above it are
the sessions', each by its agent. `fsck` prints nothing. The same store must give the same commits
every time, so a whole export into a new repository must give the same head:

```sh
git init --bare -q -b stable "$PILOT/export3.git"
swarmops git export "$PILOT/export3.git" --ref main --branch stable
git -C "$PILOT/export.git" rev-parse stable
git -C "$PILOT/export3.git" rev-parse stable
```

```text
{"ok":true,"op":"git_export","api":1,"result":{"exported":528,"objects_written":9085,"head_changeset":"431d2fe8…","head_sha":"df80904e…","branch":"stable","branch_moved":true,"incremental":false,"rewritten":0},"meta":{"repo":"…","elapsed_ms":3611,…}}
df80904e9584a7931c4e064929b71c666915a9e1
df80904e9584a7931c4e064929b71c666915a9e1
```

## Measure

| What | Where it comes from |
| --- | --- |
| Time per commit and per land | Each commit and land result's `meta.elapsed_ms`, the server's own time (the transcript, or the scripted client's lines, which add the wall time of each call) |
| Conflicts | Each `E_CONFLICT`: its paths (`details.report.files[].path`; the scripted client prints them in its `conflicts` line), how the agent resolved them (`land`'s `resolutions`), and whether the next land passed |
| Retries | `land`'s `attempts`, and every `E_CAS_MISMATCH`, `E_BUSY` and `E_CAS_RETRY_EXHAUSTED` |
| Errors an agent met | Every result with `ok: false`: its `code`, `retryable`, and what the agent did next |
| Sessions at once | For each session of step 8 (its `session-N.out`): the `wall_ms` and `elapsed_ms` of its commit and land, its `attempts`, `status` and exit code, and the `warnings` and `meta.recovered` of any line that has them |
| The killed session | The lines it printed before the kill, and the `status` of its second and third runs |
| Keys | For each session's key, how many changesets on `main` carry it |
| Verify results | `objects_checked`, `changesets_checked`, `problems` and `findings` of `verify --deep`, after steps 2, 4, 7 and 8 |
| Import and export | `imported`, `skipped`, `lossy_total`, `ref_moved` and `elapsed_ms` of each import; `exported`, `objects_written`, `incremental`, `rewritten` and `elapsed_ms` of each export |
| The round trip | Original commits missing from the export, the new commits and their authors, `git fsck`, the second export's head; after step 9 also the person's commit, in the export under the id it was pushed with |
| The gate and CI | `just check`'s exit code and test counts; the `tests-pass` conclusion of the pushed branch and of `$START` |
| The host | The load average before and after each step, the store's size and file count |

For comparison, the dry run on 2026-10-20 measured, at a load average of 80 to 130 from other
work on the same 32-thread host: import 3.8 s for 422 commits; through MCP, a commit in 49 ms and a
land in 64 ms (`elapsed_ms`); `verify --deep` 0.2 to 0.3 s for 7,647 to 7,652 objects; export
6.1 s for 423 commits and 7,651 objects; the gate 187 s from a cold build. An earlier run at a load
of 150 to 500 took 9.8 s, 105 ms, 131 ms and 12.0 s for the same steps.

A later run the same day, on the 522-commit `stable` at a load average of 42 to 46, measured: the
second import 135 ms for the person's commit; through MCP, with four sessions at once, commits of
31 to 58 ms and lands of 20 to 65 ms (`elapsed_ms`); the incremental export 28 ms for 4 commits and
12 objects, and a whole export into a new repository 3.6 s for 528 commits and 9,085 objects;
`verify --deep` 0.26 to 0.28 s for 9,074 to 9,097 objects.

## Success criteria

The pilot passes when every one of these holds; the report gives each one `yes`, `no` or
`unmeasured` with its evidence.

1. The import takes the whole history: `imported` equals the commit count of `$START`,
   `lossy_total` is 0, and the command exits 0.
2. `verify --deep` finds nothing (`problems` and `findings` empty) after the import and after the
   agent session.
3. The agent commits and lands the change through `swarmops mcp`: the land's `status` is `landed`
   (or `replayed`, on a retry with the same key), the landed changeset's `author.agent` and
   `lander.agent` are `pilot/agent-1`, and every error a tool returned is in the report, none of
   them `E_INTERNAL`, `E_STORE_CORRUPT` or `E_IO`.
4. The round trip holds: no commit of `$START`'s history is missing from the exported `stable`,
   the export's `rewritten` is 0, `git fsck --strict` passes, and a second export into a new
   repository gives the same head.
5. The exported head passes the repository's CI: `just check` exits 0 on its checkout, and
   `tests-pass` succeeds on the pushed `feat/` branch.
6. No envelope or tool result carries a filesystem path the caller did not give: the store's path
   in `meta.repo` and the git repository's path the caller passed are the only ones.
7. The second import takes in what the trunk gained: `imported` equals the number of commits the
   person pushed, `lossy_total` is 0, `main` moves to the person's commit, and `verify --deep`
   finds nothing.
8. Every key is on `main` exactly once: after step 8, each session's key, the killed session's
   too, is on one changeset of `main`. The sessions that were not killed end `landed`; the killed
   one's second run ends `landed` or `replayed`, and its third `replayed`.
9. `verify --deep` finds nothing after the sessions, the killed one included.
10. Every retry and error is in the report: each session's `attempts`, each line with `ok: false`
    with its `code` and what the session did next, and any `warnings` or `meta.recovered`; the
    only `E_CONFLICT` is the shared line's, and none of the codes is `E_INTERNAL`,
    `E_STORE_CORRUPT` or `E_IO`.
11. The second round trip holds: the person's commit is in the exported `stable` under the id it
    was pushed with, the export is incremental (`incremental` and `branch_moved` true) with
    `rewritten` 0, the commits above the person's are the sessions' four, `git fsck --strict`
    passes, and an export into a new repository gives the same head.
12. The rollback below leaves GitHub as it was: no pilot branch, and `stable` where the pilot found
    it or moved only by the usual landings.

## Rollback

Until the pilot passes, git is the source of truth, and the pilot changed nothing there but its
own branch. Rolling back deletes that branch, checks what the remote holds, and removes the pilot's
directory, store included: the store holds nothing that git needs, since its history came from
git and the session's change is on the pushed branch. If the maintainers want that change, it
lands on `stable` the usual way before the branch is deleted, after its review, as a commit by
the person who lands it: the exported commit's author is the agent's identity
(`pilot/agent-1 <agent-1@pilot.swarmops.local>`). The commits of steps 7 to 9 are in `export.git`
only: nothing of them was pushed, and the rollback removes them with the rest.

```sh
git push git@github.com:KeiaiLab/SwarmOps.git --delete "$BRANCH"
git ls-remote git@github.com:KeiaiLab/SwarmOps.git "refs/heads/$BRANCH" refs/heads/stable
rm -rf "$PILOT"
```

The dry run deleted its branch from its stand-in, and read GitHub's refs with the same
`git ls-remote`:

```text
92191bd495fae7496c9b6a64ab2891f63fbfc4f4	refs/heads/stable
```

`git ls-remote` must print only `stable`, at `$START` or at a commit the usual landings put there
since.

## Appendix: the scripted session

`mcp_session.py` speaks MCP to `swarmops mcp` over stdin and stdout, one JSON-RPC message a line.
It starts the server that a `.mcp.json` names (expanding `${VAR}` and `${VAR:-default}` in its
arguments and environment, as Claude Code does), opens the session with
`initialize`, lists the tools, then calls `ref_list`, `read_file`, `commit`, `land` and `show`. It
prints one JSON line per call, with the call's `warnings` and `meta.recovered` when it has any, and
stops at the first error, printing it. It reads the whole file, one `read_file` call per page of up
to 32 KiB, and compares it with the new content, line endings included, before the commit: when
the two are the same, it stops with a message instead, since `commit` refuses a change that changes
nothing (`E_EMPTY_COMMIT`). Save it as `$PILOT/mcp_session.py`; step 3 gives it four arguments: the
`.mcp.json`, the file's path in the repository, the file that holds the new content, and the commit
message.

Step 8 adds three options. `--key` is a uuid v7 that the commit and the land take as their
idempotency key (a commit takes no other), so that the same command run again makes the same
commit and lands it once; the commit line says `replayed` when the store held that commit already.
`--parent` is the full id of the changeset to read the file at and to commit on, in place of
`main`'s head, so that every run of the command starts from the same place. `--resolve` names a
JSON file `{"files": {<path>: <resolution>}}`: when the land fails with `E_CONFLICT`, the session
prints the paths that conflict, adds `against` from the report's `theirs`, and lands again with
those resolutions. Any other error still ends the session.

```python
"""One scripted agent session through `swarmops mcp`: read a file at main, commit a new version
of it on the head it read, land it, and print one JSON line per tool call. A new version that is
the file as it was stops the session before the commit.

--key is a uuid v7, the idempotency key of the commit and of the land: the same command run
again makes the same commit and lands it once. --parent is the full id of the changeset to read
and commit on, instead of main's head. --resolve names a JSON file {"files": {path: resolution}}
to land with when the land conflicts, against the head of main that the conflict report names."""
import argparse, base64, json, os, re, subprocess, sys, time, uuid

ap = argparse.ArgumentParser(description=__doc__)
for name in ("mcp_json", "path", "new_file", "message", "--key", "--parent", "--resolve"):
    ap.add_argument(name)
opt = ap.parse_args()
server = json.load(open(opt.mcp_json))["mcpServers"]["swarmops"]
expand = lambda v: re.sub(r"\$\{(\w+)(?::-([^}]*))?\}",
                          lambda m: os.environ.get(m[1]) or (m[2] or ""), v)
env = {**os.environ, **{k: expand(v) for k, v in server.get("env", {}).items()}}
proc = subprocess.Popen([server["command"], *map(expand, server["args"])], env=env, text=True,
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE)
ids = iter(range(1, 1_000_000))

def rpc(method, params=None):
    msg = {"jsonrpc": "2.0", "method": method}
    if params is not None:
        msg["params"] = params
    if not method.startswith("notifications/"):
        msg["id"] = next(ids)
    proc.stdin.write(json.dumps(msg) + "\n"); proc.stdin.flush()
    return json.loads(proc.stdout.readline()) if "id" in msg else None

def call(tool, args, allow=()):
    start = time.monotonic()
    env = rpc("tools/call", {"name": tool, "arguments": args})["result"]["structuredContent"]
    notes = {"replayed": (env.get("result") or {}).get("replayed"), "warnings": env.get("warnings"),
             "recovered": env["meta"].get("recovered")}
    print(json.dumps({"tool": tool, "wall_ms": round((time.monotonic() - start) * 1000),
                      "elapsed_ms": env["meta"]["elapsed_ms"], "ok": env["ok"],
                      "code": None if env["ok"] else env["error"]["code"],
                      **{k: v for k, v in notes.items() if v}}), flush=True)
    if env["ok"]:
        return env["result"]
    if env["error"]["code"] not in allow:
        sys.exit(json.dumps(env["error"]))
    return env["error"]

rpc("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                   "clientInfo": {"name": "pilot-session", "version": "1"}})
rpc("notifications/initialized")
print(json.dumps({"tools": len(rpc("tools/list")["result"]["tools"])}))
head = opt.parent or next(r["target"] for r in call("ref_list", {})["refs"]
                          if r["name"] == "main")
old, offset = b"", 0
while True:  # the whole file: read on from next_offset while truncated
    page = call("read_file", {"rev": head, "path": opt.path, "offset": offset, "max_bytes": 32768})
    old += (base64.b64decode(page["content"]) if page["encoding"] == "base64"
            else page["content"].encode("utf-8"))
    if not page["truncated"]:
        break
    offset = page["next_offset"]
new = open(opt.new_file, encoding="utf-8", newline="").read()
at = "the parent" if opt.parent else "main's head"
if new.encode("utf-8") == old:
    sys.exit(f"{opt.path}: the new content is the same as at {at}: make the change first "
             "(nothing was committed)")
commit = call("commit", {"message": opt.message, "parent": head,
                         "changes": [{"path": opt.path, "op": "put", "content": {"inline": new}}],
                         **({"idempotency_key": opt.key} if opt.key else {})})
land_req = {"head": commit["changeset"], "idempotency_key": opt.key or str(uuid.uuid4())}
land = call("land", land_req, allow=("E_CONFLICT",) if opt.resolve else ())
if "code" in land:  # a conflict, and resolutions to land it with: against the head it names
    report = land["details"]["report"]
    print(json.dumps({"conflicts": [f["path"] for f in report["files"]],
                      "against": report["theirs"]}), flush=True)
    resolutions = {**json.load(open(opt.resolve)), "against": report["theirs"]}
    land = call("land", {**land_req, "resolutions": resolutions})
call("show", {"rev": land["landed"], "max_changes": 20})
print(json.dumps({"base": head, "changeset": commit["changeset"], "landed": land["landed"],
                  "status": land["status"], "attempts": land["attempts"]}))
proc.stdin.close(); proc.wait()
```
