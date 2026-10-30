# Build and test a change

Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages).

## Before you start

- A store with a `main`, and an identity for the agent (`--as` or `SWARMOPS_AGENT`).
- The project's build and test command (here its `check.sh`), and optionally the same command in
  `.swarmops/workspaces.json` in the store's tree, so that a checked land finds it by itself:

  ```json
  {"check": {"argv": ["./check.sh"]}}
  ```

A workspace is a directory holding a changeset's tree, or only the paths under `--prefix`, and
`.swarmws/`, its private state. The agent edits, builds and tests in it with the project's own
tools; `ws land --check` then lands exactly the tree the check ran on.

## 1. Create a workspace

```sh
swarmops --as agent/worker-1 --human ws create worker-1 --path work/worker-1 --all --status-after none
```

```text
name: worker-1
path: …/work/worker-1
base: a9f3ff05…
head: a9f3ff05…
all: true
created: true
```

The same command again is the same workspace (`created: false`). Without `--all`, give
`--prefix <path>` once for each subtree to write out: the rest of the tree is absent, not deleted,
and `ws materialize <path>` writes more of it later. A new workspace also gets the files that
`.worktreeinclude` lists, copied from `--from <dir>` (gitignored files such as `.env`), and runs the
repository's bootstrap command, `bootstrap.argv` of `.swarmops/workspaces.json`;
`SWARMOPS_WS_BOOTSTRAP=skip` skips both.

A command run inside the workspace finds its store through `.swarmws/workspace.json`. The
workspace operations (`ws …` and the MCP workspace tools) act as the workspace's creator unless
`--as` or `SWARMOPS_AGENT` names another; the other commands that change the store, such as
`commit`, `land` and `sync`, still need `--as` or `SWARMOPS_AGENT` there. `--repo` and
`SWARMOPS_REPO` still come first, and a relative `SWARMOPS_REPO` is read against the working
directory: give it absolute, or unset it, before you `cd` in.

## 2. Edit, build and test

The agent edits the files with whatever it uses. `ws status` lists what changed against the
workspace's head, and how far `main` has moved since its base:

```sh
cd work/worker-1
swarmops --human ws status
```

```text
name: worker-1
base: a9f3ff05…
head: a9f3ff05…
main: a9f3ff05…
behind_by: 0
state: clean
changes[0].path: src/lib.rs
changes[0].kind: modified
changes[0].size: 53
truncated: false
```

`ws diff` shows the edits as `swarmops diff` shows a diff, and `ws discard [<path>…]` restores
paths from the head. An untracked file that `.gitignore` or `.swarmignore` matches is no change,
so build output can stay in the workspace.

## 3. Land the tested tree

```sh
swarmops --human ws land -m "greet the world" --check --key "$(uuidgen)"; echo "exit $?"
```

`-m` commits the edits first. `--check` then rebases the workspace onto `main`'s head, runs the
check in the workspace (the command after `--`, else `check.argv` of `.swarmops/workspaces.json`),
and lands only if `main` has not moved since the rebase, so the landed tree is the tree the check
ran on. A moved `main` means rebase and check again, up to `--max-attempts` (8). Here the check
fails:

```text
error: E_CHECK_FAILED: the check exited with 1: nothing landed, the workspace stands on the rebased changeset
details.changeset: 43accfef…
details.exit_code: 1
details.log: .swarmws/checks/1.log
details.stage: check
details.stderr_tail: ""
details.stdout_tail: ""
details.timed_out: false
hint: see .swarmws/checks/1.log; fix, ws commit, then ws land --check again
exit 1
```

Nothing landed, and the workspace stands on the rebased changeset, where the failure reproduces.
The check runs without a shell and without stdin, with `SWARMOPS_WS`, `SWARMOPS_CHECK_TREE` and
`SWARMOPS_CHECK_ATTEMPT` set, and is killed with its process group after `--check-timeout` seconds
(900). Fix, commit, and land again:

```sh
swarmops --human ws commit -m "spell world" --status-after none
swarmops --human ws land --check --status-after none
```

```text
changeset: 1ef0ae45…
parent: 43accfef…
files_changed: 1
blobs_written: 1
replayed: false
land.status: landed
land.landed: ae3d9647…
land.rebased_onto: a9f3ff05…
land.target: main
land.base: a9f3ff05…
land.attempts: 1
land.relocated: false
land.files_changed: 1
land.squashed_from[0]: 43accfef…
land.squashed_from[1]: 1ef0ae45…
attempts: 1
```

Without `--check`, `ws land` merges the chain onto `main` as `swarmops land` does, whatever `main`
gained since. A rebase that conflicts stops with `E_CONFLICT` and the workspace in state
`rebasing`: each conflicting text file holds diff3 markers; edit them, `ws resolve <path>`, then
`ws rebase --continue` (or `ws rebase --abort`).

## 4. Remove the workspace

```sh
cd ../..
swarmops --human ws delete worker-1
```

```text
deleted: worker-1
head: ae3d9647…
```

A workspace with edits or commits that have not landed is `E_WS_DIRTY`: `--rescue` commits the
edits as a rescue changeset first and prints its id (no ref moves), and `--force` deletes anyway.
`ws list --check` reports a workspace whose directory is gone as an orphan.

## Without workspaces: a git worktree

An agent can also build and test in a git worktree of the git side of the
[daily loop](daily-loop.md) ([a bare clone](daily-loop.md#without-a-remote-a-bare-clone),
[step 2](daily-loop.md#2-agents-commit-and-land)), with `GIT_SIDE` and its `field` helper. The
script that starts the agent exports first, so that the review branch holds `main`'s head, and
takes the pair that the export prints: the commit to check out, and the changeset it is.

```sh
EXPORT=$(swarmops git export "$GIT_SIDE" --ref main --branch review)
SHA=$(printf '%s\n' "$EXPORT" | field result.head_sha)
PARENT=$(printf '%s\n' "$EXPORT" | field result.head_changeset)
git -C "$GIT_SIDE" worktree add --detach ~/work/worker-1 "$SHA"
(cd ~/work/worker-1 && ./check.sh)
```

```text
Preparing worktree (detached HEAD 0eb07c3)
HEAD is now at 0eb07c3 a shorter greeting
check: ok
```

The worktree is detached because git keeps `git fetch` from moving a branch that a worktree has
checked out. Then the agent commits the files as they are on disk: one `put` per changed file,
each named by its absolute path, which the server reads itself
([The tools agents get](agents.md#the-tools-agents-get)), with `parent` the changeset the
worktree is:

```json
{"message": "greet the world", "parent": "1092352e3a895f0ae9301a5e0637b74ae36763b57d3e907897e0e0408a07830b", "changes": [{"path": "src/lib.rs", "op": "put", "content": {"file": "/home/me/work/worker-1/src/lib.rs"}}]}
```

What ran in the worktree is that parent's tree with the edits. To land exactly that tree,
`swarmops rebase` the commit onto `main`: `up_to_date` means `main` has not moved, and
`land --expect-target` with `main`'s id publishes the tested tree, or fails with `E_CAS_MISMATCH`
and writes nothing if `main` moved meanwhile.

```sh
cat > change.json <<EOF
[{"path": "src/lib.rs", "op": "put", "content": {"file": "$HOME/work/worker-1/src/lib.rs"}}]
EOF
C=$(swarmops --as agent/worker-1 commit -m "greet the world" --changes change.json --parent "$PARENT" | field result.changeset)
swarmops --as agent/worker-1 rebase "$C" | field result.status
swarmops --as agent/worker-1 land "$C" --expect-target "$PARENT" --key "$(uuidgen)" | field result.status
git -C "$GIT_SIDE" worktree remove --force ~/work/worker-1
```

```text
up_to_date
landed
```

A rebase that says `rebased` merged the change onto a `main` that moved: its new changeset's tree
was not tested. Export again, check it out, and test it before landing with `--expect-target` set
to the `onto` the rebase printed; or land without `--expect-target`, as a plain `land` merges onto
whatever `main` holds, and let CI on the review branch test the merge.

- A `put` writes a regular file unless it says `"mode": "exec"` (or `"symlink"`): name the mode of
  every script, or the commit drops its executable bit.
- `worktree remove` needs `--force` here, because git sees the agent's edits as uncommitted. A
  killed agent leaves its worktree registered in the git side: `git -C "$GIT_SIDE" worktree list`
  shows it, as `prunable` once its directory is gone, and `worktree prune` forgets it.
