# Build and test a change

Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages).

## Before you start

- The git side, `GIT_SIDE`, and the `field` helper of the [daily loop](daily-loop.md)
  ([a bare clone](daily-loop.md#without-a-remote-a-bare-clone), [step 2](daily-loop.md#2-agents-commit-and-land)).
- The project's build and test command (here its `check.sh`).

There are no workspaces yet (M5), so an agent that must build or test its change before it lands
does so in a git worktree of the git side, at the head it read, and commits the files it made there.
The script that starts the agent exports first, so that the review branch holds `main`'s head, and
takes the pair that the export prints: the commit to check out, and the changeset it is.

## 1. Check out main's head in a worktree

```sh
EXPORT=$(swarmops git export "$GIT_SIDE" --ref main --branch review)
SHA=$(printf '%s\n' "$EXPORT" | field result.head_sha)
PARENT=$(printf '%s\n' "$EXPORT" | field result.head_changeset)
git -C "$GIT_SIDE" worktree add --detach ~/work/worker-1 "$SHA"
```

```text
Preparing worktree (detached HEAD 0eb07c3)
HEAD is now at 0eb07c3 a shorter greeting
```

The worktree is detached because git keeps `git fetch` from moving a branch that a worktree has
checked out.

## 2. Build and test

The agent edits, builds and tests in it with whatever the project uses, here its `check.sh`:

```sh
(cd ~/work/worker-1 && ./check.sh)
```

```text
check: ok
```

## 3. Commit, land and remove the worktree

Then it commits the files as they are on disk: one `put` per changed file, each named by its
absolute path, which the server reads itself
([The tools agents get](agents.md#the-tools-agents-get)), with `parent` the changeset the
worktree is. The `commit` tool takes these arguments, and `land` the changeset it returns as its `head`:

```json
{"message": "greet the world", "parent": "1092352e3a895f0ae9301a5e0637b74ae36763b57d3e907897e0e0408a07830b", "changes": [{"path": "src/lib.rs", "op": "put", "content": {"file": "/home/me/work/worker-1/src/lib.rs"}}]}
```

The same requests at the command line, and the worktree's removal after the land:

```sh
cat > change.json <<EOF
[{"path": "src/lib.rs", "op": "put", "content": {"file": "$HOME/work/worker-1/src/lib.rs"}}]
EOF
swarmops --as agent/worker-1 commit -m "greet the world" --changes change.json --parent "$PARENT"
swarmops --as agent/worker-1 land eac1712c5c1223fc75a5a022f695f8d7838641426072452150bb26d684b8a3e9 --key "$(uuidgen)"
git -C "$GIT_SIDE" worktree remove --force ~/work/worker-1
```

```text
{"ok":true,"op":"commit","api":1,"result":{"changeset":"eac1712c5c1223fc75a5a022f695f8d7838641426072452150bb26d684b8a3e9","parent":"1092352e…","root":"24c30d20…","files_changed":1,"blobs_written":1,"replayed":false},"meta":{"repo":"…","elapsed_ms":30,…}}
{"ok":true,"op":"land","api":1,"result":{"status":"landed","landed":"380650cb0351378d43c98d50298949cbd63a4ad4f1dfdfdbc2dbfde1d960a379","rebased_onto":"1092352e…","target":"main","base":"1092352e…","attempts":1,"relocated":false,"files_changed":1,"squashed_from":["eac1712c…"]},"meta":{"repo":"…","elapsed_ms":20,…}}
```

## What to keep in mind

- `parent` is the changeset the worktree stands for, which is `main`'s head only if nothing landed
  since the export. `land` merges the change onto the head it finds, three-way, as it does for any
  commit. What ran in the worktree is the base, not that merge: CI on the review branch tests the
  merge.
- A `put` writes a regular file unless it says `"mode": "exec"` (or `"symlink"`): name the mode of
  every script, or the commit drops its executable bit.
- `worktree remove` needs `--force` here, because git sees the agent's edits as uncommitted. A
  killed agent leaves its worktree registered in the git side: `git -C "$GIT_SIDE" worktree list`
  shows it, as `prunable` once its directory is gone, and `worktree prune` forgets it.
