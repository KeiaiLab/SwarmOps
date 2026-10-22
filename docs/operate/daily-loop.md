# The daily loop

The loop keeps two copies of the history in step: the store, where agents commit and land, and
the git repository that people, CI and deployment use. The store's side of git is a bare clone
of that repository, the *git side*: imports read it, exports write it, and `git fetch` and
`git push` move commits between it and the remote. The examples call the repository `acme`, its
trunk `stable`, and its remote `git@github.com:acme/acme.git` (a stand-in: the runs behind this
guide used a local bare repository as the remote).

Until a review is part of SwarmOps (M7), a person reviews what agents landed before it reaches a
trunk that deploys. Exports therefore go to a *review branch*, `review` in the examples, where CI
and the reviewer see them, and the trunk moves to the reviewed commit by a fast-forward push
([step 5](#5-review-then-move-the-trunk)). Nothing in SwarmOps stops an export to the trunk; the
loop below never makes one.

Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages).

```text
  remote (CI, deployment)       git side (bare clone)              store
  -----------------------       ---------------------              -----
  stable ---- git fetch ----->  stable ---- git import ------->  main  <-- agents commit and land
  review <--- git push -------  review <--- git export --------  main
  stable <--- git push review:stable, after the review and green CI on review
```

## Before you start

- A store and an identity of your own ([Stores](stores.md)), and agents that reach the store
  ([Give agents the store](agents.md)).
- git on the host, with access to the remote, and `python3` for the `field` helper of step 2.
- CI that runs on a push of the review branch ([step 4](#4-send-it-to-git-for-review)).

## Once: bring the repository in

```sh
export SWARMOPS_REPO=~/.local/state/swarmops/repos/acme
export SWARMOPS_AGENT=operator/ada
GIT_SIDE=~/.local/state/swarmops/git/acme.git
git clone --bare git@github.com:acme/acme.git "$GIT_SIDE"
swarmops git import "$GIT_SIDE" --ref stable --into main
```

```json
{"ok":true,"op":"git_import","api":1,"result":{"imported":3,"skipped":0,"head_changeset":"c893148f…","git_head_sha":"84226f3190a555751039017a08ef8485fd9f514b","target_ref":"main","ref_moved":true,"lossy":[],"lossy_total":0},"meta":{"repo":"…","elapsed_ms":80,"resolved":{},"auth":"trust"}}
```

The store's `main` now holds the trunk's history, each changeset the git commit it came from.
`imported` counts the commits that became changesets, and `skipped` the commits the store held
already that the import's walk met at its frontier and stopped at, not the history the store
holds (0 in a new store).
`--ref` and `--into` name the git side's branch and the store's ref (`--ref` defaults to `HEAD`,
`--into` to `main`). Content a changeset cannot hold as git has it (a submodule, a name that is
not UTF-8, a mode or a tree order git does not write, an empty directory, a commit whose bytes
its fields cannot give back) is refused with `E_GIT_IMPORT_LOSSY`, naming the commit, the path
and the class, until you allow that class with `--lossy`
([README](../../README.md#bring-a-git-repository-in)). SHA-256 repositories, reftable refs and
shallow clones are refused.

## 1. Take in what git gained

```sh
git -C "$GIT_SIDE" fetch origin stable:stable
swarmops git import "$GIT_SIDE" --ref stable --into main
```

```json
{"ok":true,"op":"git_import","api":1,"result":{"imported":0,"skipped":1,"head_changeset":"c893148f…","git_head_sha":"84226f3190a555751039017a08ef8485fd9f514b","target_ref":"main","ref_moved":false,"old_head":"c893148f…","lossy":[],"lossy_total":0},"meta":{"repo":"…","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

With nothing new, the import writes nothing. Its walk stopped at once, at the commit `main` holds:
that one commit is the `skipped` 1. Commits pushed to the trunk on top of the last promoted export
come in, and `main` moves to the last of them (`"imported":1` for one). The fetch moves the git
side's `stable` only forward, and nothing else moves it, since exports go to the review branch. When
it refuses (`! [rejected] stable -> stable (non-fast-forward)`, exit 1), the git side's `stable`
holds commits the remote lacks: an export went to it instead of the review branch, or the remote's
trunk was rewritten.

While an export waits for its review, the trunk is behind `main`, and the import fails with
`E_GIT_IMPORT_DIVERGED` although nothing is wrong: take this step after step 5, once the trunk holds
the last export ([One writer per trunk](one-writer-per-trunk.md) tells this from a real divergence).

## 2. Agents commit and land

Agents do this through the `commit` and `land` tools. The same requests at the command line, as an
operator or a script makes them, read `main`'s head, commit a change on it, and land it. `field`
prints one field of an envelope:

```sh
field() { python3 -I -c 'import functools, json, sys; print(functools.reduce(lambda v, k: v[k], sys.argv[1].split("."), json.load(sys.stdin)))' "$1"; }
BASE=$(swarmops resolve main | field result.id)
cat change.json
swarmops --as agent/worker-1 commit -m "greet the swarm" --changes change.json --parent "$BASE"
swarmops --as agent/worker-1 land 8f22e3a1ee2ff15836609fdf359065ce00cccab04d858258e9b1d8cb50c40af1 --key "$(uuidgen)"
```

```text
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn greet() -> &'static str {\n    \"hello, swarm\"\n}\n"}}]
{"ok":true,"op":"commit","api":1,"result":{"changeset":"8f22e3a1ee2ff15836609fdf359065ce00cccab04d858258e9b1d8cb50c40af1","parent":"c893148f…","root":"ff7ba3ee…","files_changed":1,"blobs_written":1,"replayed":false},"meta":{"repo":"…","elapsed_ms":146,…}}
{"ok":true,"op":"land","api":1,"result":{"status":"landed","landed":"8fe1b0f9749d40af77452e714b5183da151033b004ecbd5d26cc0c3bdf805138","rebased_onto":"c893148f…","target":"main","base":"c893148f…","attempts":1,"relocated":false,"files_changed":1,"squashed_from":["8f22e3a1…"]},"meta":{"repo":"…","elapsed_ms":146,…}}
```

The commit records the change without moving any ref; the land merges it onto `main`'s head
three-way and moves `main` to the result in one transaction, merging again by itself when `main`
moved meanwhile. The `--key` (here a random uuid, which `land` takes: 1 to 64 letters, digits and
`-`) makes a retry of the same land a replay instead of a second land.

A second agent changed the same line on the same base. Its land merges onto the first agent's
result, meets the conflict, and records nothing:

```sh
cat change2.json
C2=$(swarmops --as agent/worker-2 commit -m "a shorter greeting" --changes change2.json --parent "$BASE" | field result.changeset)
swarmops --as agent/worker-2 land "$C2" --key "$(uuidgen)" --pretty
```

```text
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn greet() -> &'static str {\n    \"hi\"\n}\n"}},
 {"path": "README.md", "op": "put", "content": {"inline": "# acme\n\nA small service that greets, briefly.\n"}}]
{
  "ok": false,
  "op": "land",
  "api": 1,
  "error": {
    "code": "E_CONFLICT",
    "message": "1 path(s) of the change conflict with the target's head 8fe1b0f9749d40af77452e714b5183da151033b004ecbd5d26cc0c3bdf805138: nothing was recorded",
    "retryable": false,
    "exit": 3,
    "details": {
      "report": {
        "base": "c893148f550e68a1d0c0dfcf9bd10b4a738b8f60245ddcc94f4dbf0cb5d67755",
        "files": [
          {
            "base_blob": "2cda0801…",
            "hunks": [
              {
                "base": {
                  "first": 2,
                  "last": 2,
                  "text": "    \"hello\"\n"
                },
                "id": 0,
                "ours": {
                  "first": 2,
                  "last": 2,
                  "text": "    \"hi\"\n"
                },
                "theirs": {
                  "first": 2,
                  "last": 2,
                  "text": "    \"hello, swarm\"\n"
                }
              }
            ],
            "kind": "content",
            "ours_blob": "a3aeb84a…",
            "path": "src/lib.rs",
            "theirs_agent": "agent/worker-1",
            "theirs_blob": "5d417d12…",
            "theirs_changeset": "8fe1b0f9749d40af77452e714b5183da151033b004ecbd5d26cc0c3bdf805138"
          }
        ],
        "kinds": [
          "content"
        ],
        "ours": "8070d2dd63f46a1157fb28ccc7025c7b6d3c39be621930bf6dbfee44075aef7f",
        "theirs": "8fe1b0f9749d40af77452e714b5183da151033b004ecbd5d26cc0c3bdf805138",
        "truncated": false
      }
    },
    "hint": "swarmops cat 8fe1b0f9…:src/lib.rs, then swarmops land 8070d2dd… --resolve <file> with each path's resolution and against = 8fe1b0f9…"
  },
  …
}
```

`README.md`, which only the second agent changed, would apply as it is; `src/lib.rs`, whose same
line both changed, conflicts. The report gives each side's lines, numbered as `cat --lines` takes
them, and the agent that last changed the path on `main`.

The agent resolves it without a checkout, against the report's `theirs`, and lands again:

```sh
cat resolve.json
swarmops --as agent/worker-2 land "$C2" --resolve resolve.json --key "$(uuidgen)"
```

```text
{"against": "8fe1b0f9749d40af77452e714b5183da151033b004ecbd5d26cc0c3bdf805138",
 "files": {"src/lib.rs": {"content": {"inline": "pub fn greet() -> &'static str {\n    \"hi, swarm\"\n}\n"}}}}
{"ok":true,"op":"land","api":1,"result":{"status":"landed","landed":"1092352e3a895f0ae9301a5e0637b74ae36763b57d3e907897e0e0408a07830b","rebased_onto":"8fe1b0f9…","target":"main","base":"c893148f…","attempts":1,"relocated":false,"files_changed":2,"squashed_from":["8070d2dd…"]},"meta":{"repo":"…","elapsed_ms":134,…}}
```

Through MCP, the `land` tool takes the same resolutions as its `resolutions` argument.

## 3. Look at what landed

```sh
swarmops log -n 3 --human
swarmops diff "$BASE" main --format stat --human
swarmops op log -n 2 --human
```

```text
entries[0].id: 1092352e3a895f0ae9301a5e0637b74ae36763b57d3e907897e0e0408a07830b
entries[0].parents[0]: 8fe1b0f9749d40af77452e714b5183da151033b004ecbd5d26cc0c3bdf805138
entries[0].author_agent: agent/worker-2
entries[0].created_at: 1792484229000
entries[0].title: a shorter greeting
…
from: c893148f550e68a1d0c0dfcf9bd10b4a738b8f60245ddcc94f4dbf0cb5d67755
to: 1092352e3a895f0ae9301a5e0637b74ae36763b57d3e907897e0e0408a07830b
files[0].path: README.md
files[0].status: modified
files[0].binary: false
files[0].insertions: 1
files[0].deletions: 1
files[1].path: src/lib.rs
…
ops[0].op: 5
ops[0].kind: land
ops[0].changesets[0].id: 1092352e3a895f0ae9301a5e0637b74ae36763b57d3e907897e0e0408a07830b
ops[0].changesets[0].agent: agent/worker-2
…
ops[0].ref_moves[0].name: main
ops[0].ref_moves[0].old: 8fe1b0f9749d40af77452e714b5183da151033b004ecbd5d26cc0c3bdf805138
ops[0].ref_moves[0].new: 1092352e3a895f0ae9301a5e0637b74ae36763b57d3e907897e0e0408a07830b
ops[0].ref_moves[0].actor: agent/worker-2
…
```

`op log` numbers every write transaction; `swarmops op undo <op>` moves the refs one of them moved
back, by compare-and-swap, all or none ([README](../../README.md#first-commands)).

## 4. Send it to git for review

```sh
swarmops git export "$GIT_SIDE" --ref main --branch review
git -C "$GIT_SIDE" push origin review
```

```text
{"ok":true,"op":"git_export","api":1,"result":{"exported":2,"objects_written":9,"head_changeset":"1092352e…","head_sha":"0eb07c3f0140e815e6bc153043eebaf5672eb46b","branch":"review","branch_moved":true,"incremental":false,"rewritten":0},"meta":{"repo":"…","elapsed_ms":185,…}}
To …
 * [new branch]      review -> review
```

The export writes every changeset the branch does not reach as a git commit, makes the objects
durable, then moves the branch by compare-and-swap; run again, it writes nothing. This one made the
branch (`incremental` is false and there is no `old_sha`), and later ones move it forward. CI runs
on the push as it runs on any branch push (point it at this branch if it runs only on the trunk or
on pull requests), and the reviewer reads the branch. What they see:

```sh
git -C "$GIT_SIDE" log --format="%h %an <%ae>%n    %s" -3 review
git -C "$GIT_SIDE" log -1 --format=%B review
```

```text
0eb07c3 agent/worker-2 <worker-2@agent.swarmops.local>
    a shorter greeting
91ac021 agent/worker-1 <worker-1@agent.swarmops.local>
    greet the swarm
84226f3 Ada Lovelace <ada@example.com>
    docs: say what acme does
a shorter greeting

SwarmOps-Changeset: 1092352e3a895f0ae9301a5e0637b74ae36763b57d3e907897e0e0408a07830b
SwarmOps-Land-Key: 23fc28da-8ee7-4ed6-ba30-b94291a54525
SwarmOps-Squashed-From: 8070d2dd63f46a1157fb28ccc7025c7b6d3c39be621930bf6dbfee44075aef7f
```

Commits imported from git come back as themselves, byte for byte. A changeset made in the store
becomes a commit whose author and committer are the identities the store recorded for it (an
agent's, with a `swarmops.local` address), with SwarmOps' trailers after the message and no
signature: a pipeline or branch rule that requires signed commits or known authors refuses it. A
message that already ends in a trailer block, such as `Co-Authored-By:`, keeps it, and the
SwarmOps trailers join that block. Tags stay git's: tag an exported commit in git, as before.

## 5. Review, then move the trunk

A person reads what the review branch has that the trunk lacks (`git log` and
`git diff stable...review` in the git side, or a pull request from `review` to `stable`), and CI
runs on the branch. When the review approves and CI is green on the branch's tip, the trunk moves to
that commit by a fast-forward push:

```sh
git -C "$GIT_SIDE" log --oneline stable..review
git -C "$GIT_SIDE" push origin review:stable
```

```text
0eb07c3 a shorter greeting
91ac021 greet the swarm
To …
   84226f3..0eb07c3  review -> stable
```

The push makes no commit: the trunk now holds the commits that were reviewed and tested, and the CI
run of the branch's tip is the run of the trunk's. The remote's deployment still runs on the trunk,
and is to wait for green CI on that commit. When agents landed more and exported again while the
review ran, push the commit that was reviewed (`git push origin <commit>:stable`), not the branch.
The next step 1 finds the last export on the trunk, so the import continues from the exported head
and takes in nothing:

```sh
git -C "$GIT_SIDE" fetch origin stable:stable
swarmops git import "$GIT_SIDE" --ref stable --into main
```

```text
From …
   84226f3..0eb07c3  stable     -> stable
{"ok":true,"op":"git_import","api":1,"result":{"imported":0,"skipped":1,"head_changeset":"1092352e…","git_head_sha":"0eb07c3f0140e815e6bc153043eebaf5672eb46b","target_ref":"main","ref_moved":false,"old_head":"1092352e…","lossy":[],"lossy_total":0},"meta":{"repo":"…","elapsed_ms":1,…}}
```

Move the trunk with this push, not with the host's merge button: a squash merge writes one commit
that the store never exported, and the next import fails with `E_GIT_IMPORT_DIVERGED` (a merge
commit works, and comes in as a merge changeset).

Next: an agent that must build or test before it lands works in a
[git worktree](build-and-test.md); when people change the trunk too, see
[One writer per trunk](one-writer-per-trunk.md).
