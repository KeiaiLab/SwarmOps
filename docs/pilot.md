# The pilot: SwarmOps on its own repository

The first real use of SwarmOps is on SwarmOps itself. Its git history goes into a store, one agent
session commits and lands a small real change through `swarmops mcp`, the store's history goes
back into a git branch, and this repository's own CI runs on that branch. Nothing is at risk: the
repository on GitHub stays the source of truth until the pilot passes, the pilot pushes nothing but
one `feat/` branch, and removing the pilot's directory and that branch undoes all of it.

[operations.md](operations.md) explains each command. This runbook says what to run, in order,
what to measure, what counts as a pass, and how to roll back. Every command here was run in a dry
run on 2026-10-20 with the binary built from this repository, and the outputs below are copied
from it (`…` marks a cut, and the pilot directory's path is left out), except what needs a person
or write access to GitHub: the interactive Claude Code session of step 3, and the push of step 6
and its deletion in the rollback, which the dry run made to a local stand-in for the remote. Each
is marked where it comes.

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

**With Claude Code** (not run in the dry run, which ran inside a Claude Code session itself),
start a session in `$PILOT/agent`, approve the project's `swarmops` server when Claude Code asks,
and give it this prompt with the change filled in:

```sh
cd "$PILOT/agent" && claude
```

```text
Use only the swarmops tools. Read <path> at main: ref_list gives main's full changeset id, and
read_file reads at that id (max_bytes 32768 at most; read on from next_offset while truncated).
Make this change: <the change>. Commit the whole new file with parent set to the id you read, and
end the commit message with a Co-Authored-By trailer naming your model. Then land the changeset
with a new idempotency_key. Reply with the landed changeset id and every error a tool returned,
with its code.
```

Claude Code keeps the session's transcript as JSON lines under `~/.claude/projects/`. In it, the
change must go through the tools `mcp__swarmops__commit` and `mcp__swarmops__land`, and each tool
result is an envelope whose `meta.elapsed_ms` is the server's own time for that call.

**With the scripted client** (the dry run's way, and the way for an operator that is itself an
agent): write the changed file's new content to a file, then run the
[appendix](#appendix-the-scripted-session)'s `mcp_session.py`, which drives the same server through
the same `.mcp.json`: it reads the file at main's head, commits the new content on that head, lands
it, shows the result, and prints one line per tool call. The message ends with the trailer that
names the model of the agent that wrote the change, as CONTRIBUTING.md asks of every tool-assisted
commit; the export keeps it, and adds its own trailers to that block.

Save the appendix's script as `$PILOT/mcp_session.py`. It changes one file: `FILE` names it by
its path in the repository, `SUBJECT` is the change in one line, and `MODEL` the model of the
agent that makes it. Set them for your change, and start the new content from the file as
`$START` holds it:

```sh
FILE=docs/known-limitations.md    # the file your change touches: the example is yours to replace
SUBJECT='docs: …'                 # your change in one line, the commit's subject
MODEL='the coding agent'           # the model of the agent that makes the change
git -C "$PILOT/source.git" show "$START:$FILE" > "$PILOT/new-content"
```

Make the change in `$PILOT/new-content`, with an editor or with the agent's own edit, then run
the session:

```sh
python3 -I "$PILOT/mcp_session.py" "$PILOT/agent/.mcp.json" "$FILE" "$PILOT/new-content" \
  "$(printf '%s\n\nCo-Authored-By: %s <agent@example.invalid>' "$SUBJECT" "$MODEL")"
```

Before the commit, the script compares the new content with the whole file it read at main's
head. When the two are the same, as when the change was not made, it stops with a message that
says so and exits 1, and nothing is recorded: make the change, and run it again.

The dry run's lines, for its change and the subject `docs: known limitations as of M2`; its file
fit in one `read_file` page. The block as written here ran on 2026-10-20 too, against `stable` at
`23ce4a1`, whose `docs/known-limitations.md` takes two pages, and so two `read_file` lines:

```text
{"tools": 18}
{"tool": "ref_list", "wall_ms": 0, "elapsed_ms": 0, "ok": true, "code": null}
{"tool": "read_file", "wall_ms": 1, "elapsed_ms": 0, "ok": true, "code": null}
{"tool": "commit", "wall_ms": 50, "elapsed_ms": 49, "ok": true, "code": null}
{"tool": "land", "wall_ms": 65, "elapsed_ms": 64, "ok": true, "code": null}
{"tool": "show", "wall_ms": 2, "elapsed_ms": 0, "ok": true, "code": null}
{"base": "a6bcf9d8932d205f14023823bd8aadd8e3edc2b7c357bee5da57de7e227e8019", "changeset": "b0e4812ba53c76c6e3cd8541b2f5e276cfaef7efcae23420f169b6d574ffbfeb", "landed": "41b15c96e8bf50b1ecf88faf6d0f7718589220b8be2421531c8c745ff997857b", "status": "landed", "attempts": 1}
```

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

## Measure

| What | Where it comes from |
| --- | --- |
| Time per commit and per land | Each commit and land result's `meta.elapsed_ms`, the server's own time (the transcript, or the scripted client's lines, which add the wall time of each call) |
| Conflicts | Each `E_CONFLICT`: its paths (`details.report.files[].path`), how the agent resolved them (`land`'s `resolutions`), and whether the next land passed |
| Retries | `land`'s `attempts`, and every `E_CAS_MISMATCH`, `E_BUSY` and `E_CAS_RETRY_EXHAUSTED` |
| Errors an agent met | Every result with `ok: false`: its `code`, `retryable`, and what the agent did next |
| Verify results | `objects_checked`, `changesets_checked`, `problems` and `findings` of `verify --deep`, after step 2 and after step 4 |
| Import and export | `imported`, `skipped`, `lossy_total` and `elapsed_ms`; `exported`, `objects_written`, `rewritten` and `elapsed_ms` |
| The round trip | Original commits missing from the export, the new commits and their authors, `git fsck`, the second export's head |
| The gate and CI | `just check`'s exit code and test counts; the `tests-pass` conclusion of the pushed branch and of `$START` |
| The host | The load average before and after each step, the store's size and file count |

For comparison, the dry run on 2026-10-20 measured, at a load average of 80 to 130 from other
work on the same 32-thread host: import 3.8 s for 422 commits; through MCP, a commit in 49 ms and a
land in 64 ms (`elapsed_ms`); `verify --deep` 0.2 to 0.3 s for 7,647 to 7,652 objects; export
6.1 s for 423 commits and 7,651 objects; the gate 187 s from a cold build. An earlier run at a load
of 150 to 500 took 9.8 s, 105 ms, 131 ms and 12.0 s for the same steps.

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
7. The rollback below leaves GitHub as it was: no pilot branch, and `stable` where the pilot found
   it or moved only by the usual landings.

## Rollback

Until the pilot passes, git is the source of truth, and the pilot changed nothing there but its
own branch. Rolling back deletes that branch, checks what the remote holds, and removes the pilot's
directory, store included: the store holds nothing that git needs, since its history came from
git and the session's change is on the pushed branch. If the maintainers want that change, it
lands on `stable` the usual way before the branch is deleted, after its review and under
CONTRIBUTING.md's attribution rules: there the git author is the person the agent works for, while
the exported commit's author is the agent's identity
(`pilot/agent-1 <agent-1@pilot.swarmops.local>`).

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
prints one JSON line per call and stops at the first error, printing it. It reads the whole file,
one `read_file` call per page of up to 32 KiB, and compares it with the new content before the
commit: when the two are the same, it stops with a message instead, since `commit` refuses a
change that changes nothing (`E_EMPTY_COMMIT`). Save it as `$PILOT/mcp_session.py`; step 3 gives
it four arguments: the `.mcp.json`, the file's path in the repository, the file that holds the
new content, and the commit message.

```python
"""One scripted agent session through `swarmops mcp`: read a file at main, commit a new version
of it on the head it read, land it, and print one JSON line per tool call. A new version that is
the file as it was stops the session before the commit."""
import base64, json, os, re, subprocess, sys, time, uuid

mcp_json, path, new_file, message = sys.argv[1:5]
server = json.load(open(mcp_json))["mcpServers"]["swarmops"]
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

def call(tool, args):
    start = time.monotonic()
    env = rpc("tools/call", {"name": tool, "arguments": args})["result"]["structuredContent"]
    print(json.dumps({"tool": tool, "wall_ms": round((time.monotonic() - start) * 1000),
                      "elapsed_ms": env["meta"]["elapsed_ms"], "ok": env["ok"],
                      "code": None if env["ok"] else env["error"]["code"]}), flush=True)
    if not env["ok"]:
        sys.exit(json.dumps(env["error"]))
    return env["result"]

rpc("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                   "clientInfo": {"name": "pilot-session", "version": "1"}})
rpc("notifications/initialized")
print(json.dumps({"tools": len(rpc("tools/list")["result"]["tools"])}))
head = next(r["target"] for r in call("ref_list", {})["refs"] if r["name"] == "main")
old, offset = b"", 0
while True:  # the whole file: read on from next_offset while truncated
    page = call("read_file", {"rev": head, "path": path, "offset": offset, "max_bytes": 32768})
    old += (base64.b64decode(page["content"]) if page["encoding"] == "base64"
            else page["content"].encode("utf-8"))
    if not page["truncated"]:
        break
    offset = page["next_offset"]
new = open(new_file, encoding="utf-8").read()
if new.encode("utf-8") == old:
    sys.exit(f"{path}: the new content is the same as at main's head: make the change first "
             "(nothing was committed)")
commit = call("commit", {"message": message, "parent": head,
                         "changes": [{"path": path, "op": "put", "content": {"inline": new}}]})
land = call("land", {"head": commit["changeset"], "idempotency_key": str(uuid.uuid4())})
call("show", {"rev": land["landed"], "max_changes": 20})
print(json.dumps({"base": head, "changeset": commit["changeset"], "landed": land["landed"],
                  "status": land["status"], "attempts": land["attempts"]}))
proc.stdin.close(); proc.wait()
```
