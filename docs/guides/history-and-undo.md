# Reading and repairing history

This guide shows how to read a store's history without a checkout, and how to take back a ref
move that should not have happened. It names revisions, reads files and directories, looks at
changesets, walks the log and one path's history, compares two revisions, lists the store's
operations, undoes one, and checks the store. The last sections cover the three output formats,
the envelope and the exit codes.

Every command below was run with the binary built from this repository, against a scratch
store, and every output is copied from those runs. In the outputs, each 64-digit id is cut to its
first 12 hex digits and `…`; the commands print them whole, and the commands shown here take
them whole where they need a full id. The examples run `so`, the short name; `swarmops` is the
same program. `so <command> --help` is the authority on flags, and the README's
[Commands](../../README.md#commands) table lists them all.

## Set up a store to follow along

The examples read one store, `acme`, with three changesets on `main`. Reads need no identity;
the `ref` and `op undo` examples write, so they need one.

```sh
export SWARMOPS_REPO=acme               # or --repo <store> on each command
export SWARMOPS_AGENT=agent/worker-1    # or --as <family>/<instance>

so init acme --name acme
cat > c1.json <<'EOF'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() {}\n"}},
 {"path": "README.md", "op": "put", "content": {"inline": "# acme\n"}}]
EOF
cat > c2.json <<'EOF'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() {}\n\npub fn g() -> u32 {\n    1\n}\n"}}]
EOF
cat > c3.json <<'EOF'
[{"path": "docs/guide.md", "op": "put", "content": {"inline": "Call f, then g.\n"}},
 {"path": "README.md", "op": "put", "content": {"inline": "# acme\n\nSee docs/guide.md.\n"}}]
EOF
so commit -m first --changes c1.json --advance main
so commit -m 'add g' --changes c2.json --advance main
so commit -m 'add a guide' --changes c3.json --advance main

# Two change lists the undo examples use later.
cat > c4.json <<'EOF'
[{"path": "NOTES.md", "op": "put", "content": {"inline": "Release notes.\n"}}]
EOF
cat > c5.json <<'EOF'
[{"path": "README.md", "op": "delete"}]
EOF
```

The three commits made these changesets; your ids differ, because a changeset's id covers its
author and its time:

| Changeset | Message | Revision |
| --- | --- | --- |
| `3ba35c44081b…` | first | `main~2` |
| `ce8c4ae35795…` | add g | `main~1` |
| `48b8a4cf3ca9…` | add a guide | `main` |

## Name a revision

Every read takes a revision: a name for one changeset. A revision is one of these, then any
number of `~N` suffixes:

| Form | Example | Names |
| --- | --- | --- |
| A ref's name | `main`, `release/1.0` | The changeset the ref points at. |
| A full changeset id | 64 hex digits | That changeset. |
| An id prefix | `ce8c4ae35795` | The one changeset whose id starts with it: 12 to 63 hex digits, either case. |
| `~N` after any of these | `main~2`, `main~1~1` | The changeset `N` first parents back. The suffixes add up: `main~1~1` is `main~2`. |

`so resolve` says what a revision names. `kind` is `ref` when the revision is exactly a ref's
name, and `changeset` otherwise:

```sh
so resolve main --human
```

```text
id: 48b8a4cf3ca9…
kind: ref
```

```sh
so resolve main~1~1 --human
```

```text
id: 3ba35c44081b…
kind: changeset
```

A prefix reads in either case, and `~N` works after an id as well as after a ref:

```sh
so resolve CE8C4AE35795 --human
```

```text
id: ce8c4ae35795…
kind: changeset
```

```sh
so resolve 48b8a4cf3ca95a01caec61568a22e7f4f0ad89ef71b7d1c9ac85e57544b3b641~2 --human
```

```text
id: 3ba35c44081b…
kind: changeset
```

There is no `^`, no `HEAD`, no `main~` without a number, and no prefix shorter than 12 digits.
Each of them is `E_INVALID_ARG` (exit 2), and the hint gives the grammar:

```sh
so resolve main^ --human
```

```text
error: E_INVALID_ARG: "main^" is not a revision
details.rev: main^
hint: a revision is main, release/<name>, a changeset id or 12 or more of its first hex digits, then optionally ~N for the N-th first parent
```

A revision in the right form that names nothing is `E_REV_NOT_FOUND` (exit 6): a ref that does
not exist, an id or a prefix of no changeset, or a `~N` past the first changeset. The hint is
the command that shows what it could have named:

```sh
so resolve main~3 --human
```

```text
error: E_REV_NOT_FOUND: main~3: not found
details.rev: main~3
hint: swarmops log main
```

A prefix that more than one changeset starts with is `E_AMBIGUOUS_ID` (exit 1): give more
digits.

Every command that resolves a revision lists it in its envelope's `meta.resolved`, with the
full id it named, so a script can pin the id it read:

```sh
so resolve main~1
```

```text
{"ok":true,"op":"resolve_rev","api":1,"result":{"id":"ce8c4ae35795…","kind":"changeset"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{"main~1":"ce8c4ae35795…"},"auth":"trust"}}
```

A ref name or a relative revision resolves when the command runs. Where a command compares a
ref with what you read (`ref set --expect`, `ref delete --expect`), give the full id you read,
not `main`: the ref's own name always matches, so the compare-and-swap would protect nothing.

## Read a file

`so cat REV:PATH` prints one file at a revision. The revision and the path split at the first
`:`.

```sh
so cat main:src/lib.rs --human
```

```text
blob: a15b2af67626…
size: 43
kind: file
encoding: utf8
content: |
  pub fn f() {}
  
  pub fn g() -> u32 {
      1
  }
truncated: false
```

The same path two changesets back holds the first version:

```sh
so cat main~2:src/lib.rs --human
```

```text
blob: f5f5220aa3d3…
size: 14
kind: file
encoding: utf8
content: |
  pub fn f() {}
truncated: false
```

`content` is the text when the bytes are UTF-8 (`encoding: utf8`), and base64 otherwise
(`encoding: base64`). `kind` is `file`, `exec` or `symlink`.

### Read part of a file

`--lines A-B` prints lines A to B, numbered from 1, both included. `offset` says where in the
file they start:

```sh
so cat main:src/lib.rs --lines 3-5 --human
```

```text
blob: a15b2af67626…
size: 43
kind: file
encoding: utf8
content: |
  pub fn g() -> u32 {
      1
  }
truncated: false
offset: 15
lines.first: 3
lines.last: 5
```

`--max-bytes N` caps the content (1 MiB by default). A read cut short says `truncated: true`
and gives `next_offset`, where `--offset` reads on:

```sh
so cat main:src/lib.rs --max-bytes 10 --human
```

```text
blob: a15b2af67626…
size: 43
kind: file
encoding: utf8
content: pub fn f()
truncated: true
next_offset: 10
```

```sh
so cat main:src/lib.rs --offset 10 --max-bytes 10 --human
```

```text
blob: a15b2af67626…
size: 43
kind: file
encoding: utf8
content: |
   {}
  
  pub f
truncated: true
offset: 10
next_offset: 20
```

`--offset` and `--lines` do not go together.

### When the path is not a file

A directory is `E_IS_DIR` (exit 1), and nothing at the path is `E_PATH_NOT_FOUND` (exit 6). Each
hint is the `ls` to run instead:

```sh
so cat main:src --human
```

```text
error: E_IS_DIR: "src" is a directory at main
details.path: src
details.rev: main
hint: swarmops ls main:src
```

```sh
so cat main:nope.txt --human
```

```text
error: E_PATH_NOT_FOUND: nothing is at "nope.txt" in main
details.path: nope.txt
details.rev: main
hint: swarmops ls main
```

## List a directory

`so ls REV` lists the root at a revision, and `so ls REV:PATH` one directory. Each file comes
with its size and blob id, each directory with its tree id:

```sh
so ls main --human
```

```text
entries[0].name: README.md
entries[0].kind: file
entries[0].size: 27
entries[0].id: a7a064662799…
entries[1].name: docs
entries[1].kind: dir
entries[1].id: cde13afef0d0…
entries[2].name: src
entries[2].kind: dir
entries[2].id: 9ceb78ad12e6…
truncated: false
```

`--depth N` lists the directories below too, up to 64 levels, each entry with its `path`.
`--limit N` (250 by default) cuts the page; a cut page says `truncated: true` and gives a
`cursor`:

```sh
so ls main --depth 2 --limit 3 --human
```

```text
entries[0].name: README.md
entries[0].path: README.md
entries[0].kind: file
entries[0].size: 27
entries[0].id: a7a064662799…
entries[1].name: docs
entries[1].path: docs
entries[1].kind: dir
entries[1].id: cde13afef0d0…
entries[2].name: guide.md
entries[2].path: docs/guide.md
entries[2].kind: file
entries[2].size: 16
entries[2].id: 5ef3ee5b733f…
truncated: true
cursor: docs/guide.md
```

Pass the cursor back, exactly as it was returned, for the next page:

```sh
so ls main --depth 2 --limit 3 --cursor docs/guide.md --human
```

```text
entries[0].name: src
entries[0].path: src
entries[0].kind: dir
entries[0].id: 9ceb78ad12e6…
entries[1].name: lib.rs
entries[1].path: src/lib.rs
entries[1].kind: file
entries[1].size: 43
entries[1].id: a15b2af67626…
truncated: false
```

## Look at one changeset

`so show REV` prints one changeset: its parents, root tree, author, time, message, the changes
it made, its metadata and its signatures. `created_at` is in Unix milliseconds, the signatures'
`secs` in Unix seconds, and `seq` is the changeset's place in the store's index, in commit order.

```sh
so show main --human
```

```text
id: 48b8a4cf3ca9…
seq: 2
parents[0]: ce8c4ae35795…
root: 4ca6750dfec7…
author.agent: agent/worker-1
created_at: 1792626460000
message: add a guide
changes[0].path: README.md
changes[0].op: put
changes[0].kind: file
changes[0].blob: a7a064662799…
changes[1].path: docs/guide.md
changes[1].op: put
changes[1].kind: file
changes[1].blob: 5ef3ee5b733f…
files_changed: 2
changes_truncated: false
metadata.agent: agent/worker-1
signatures.author.name: agent/worker-1
signatures.author.email: worker-1@agent.swarmops.local
signatures.author.secs: 1792626460
signatures.author.offset_secs: 0
signatures.committer.name: agent/worker-1
signatures.committer.email: worker-1@agent.swarmops.local
signatures.committer.secs: 1792626460
signatures.committer.offset_secs: 0
```

`--max-changes N` (400 by default) caps the list of changes. `files_changed` still counts them
all, and `changes_truncated` says the list was cut:

```sh
so show main~2 --max-changes 1 --human
```

```text
id: 3ba35c44081b…
seq: 0
parents: []
root: 519154f76a1f…
author.agent: agent/worker-1
created_at: 1792626460000
message: first
changes[0].path: README.md
changes[0].op: put
changes[0].kind: file
changes[0].blob: 745ffc98a5d4…
files_changed: 2
changes_truncated: true
metadata.agent: agent/worker-1
signatures.author.name: agent/worker-1
signatures.author.email: worker-1@agent.swarmops.local
signatures.author.secs: 1792626460
signatures.author.offset_secs: 0
signatures.committer.name: agent/worker-1
signatures.committer.email: worker-1@agent.swarmops.local
signatures.committer.secs: 1792626460
signatures.committer.offset_secs: 0
```

`parents: []` marks the first changeset of a history.

## Walk the history

`so log` lists history along first parents, newest first, from `main` by default. Each entry
gives the id, the parents, the agent, the time and the message's first line:

```sh
so log --human
```

```text
entries[0].id: 48b8a4cf3ca9…
entries[0].parents[0]: ce8c4ae35795…
entries[0].author_agent: agent/worker-1
entries[0].created_at: 1792626460000
entries[0].title: add a guide
entries[1].id: ce8c4ae35795…
entries[1].parents[0]: 3ba35c44081b…
entries[1].author_agent: agent/worker-1
entries[1].created_at: 1792626460000
entries[1].title: add g
entries[2].id: 3ba35c44081b…
entries[2].parents: []
entries[2].author_agent: agent/worker-1
entries[2].created_at: 1792626460000
entries[2].title: first
truncated: false
```

### Start somewhere else

Give a revision to start from it:

```sh
so log main~1 --human
```

```text
entries[0].id: ce8c4ae35795…
entries[0].parents[0]: 3ba35c44081b…
entries[0].author_agent: agent/worker-1
entries[0].created_at: 1792626460000
entries[0].title: add g
entries[1].id: 3ba35c44081b…
entries[1].parents: []
entries[1].author_agent: agent/worker-1
entries[1].created_at: 1792626460000
entries[1].title: first
truncated: false
```

### Read it a page at a time

`-n N` (50 by default) cuts the page. A cut page gives a `cursor`: the last id it listed.

```sh
so log -n 1 --human
```

```text
entries[0].id: 48b8a4cf3ca9…
entries[0].parents[0]: ce8c4ae35795…
entries[0].author_agent: agent/worker-1
entries[0].created_at: 1792626460000
entries[0].title: add a guide
truncated: true
cursor: 48b8a4cf3ca9…
```

```sh
so log -n 1 --cursor 48b8a4cf3ca95a01caec61568a22e7f4f0ad89ef71b7d1c9ac85e57544b3b641 --human
```

```text
entries[0].id: ce8c4ae35795…
entries[0].parents[0]: 3ba35c44081b…
entries[0].author_agent: agent/worker-1
entries[0].created_at: 1792626460000
entries[0].title: add g
truncated: true
cursor: ce8c4ae35795…
```

A cursor is a full changeset id; anything else is `E_INVALID_ARG` (exit 2).

### Only the changesets that touch some paths

`--path P` keeps the changesets that change `P` or anything below it. Give it again for more
paths; a changeset that changes any of them is listed:

```sh
so log --path src --human
```

```text
entries[0].id: ce8c4ae35795…
entries[0].parents[0]: 3ba35c44081b…
entries[0].author_agent: agent/worker-1
entries[0].created_at: 1792626460000
entries[0].title: add g
entries[1].id: 3ba35c44081b…
entries[1].parents: []
entries[1].author_agent: agent/worker-1
entries[1].created_at: 1792626460000
entries[1].title: first
truncated: false
```

```sh
so log --path docs --path README.md --human
```

```text
entries[0].id: 48b8a4cf3ca9…
entries[0].parents[0]: ce8c4ae35795…
entries[0].author_agent: agent/worker-1
entries[0].created_at: 1792626460000
entries[0].title: add a guide
entries[1].id: 3ba35c44081b…
entries[1].parents: []
entries[1].author_agent: agent/worker-1
entries[1].created_at: 1792626460000
entries[1].title: first
truncated: false
```

## Follow one path

`so history REV:PATH` lists the changesets that changed a file, or anything below a directory,
newest first, along first parents. Each entry says what the path holds after that changeset:
`op` (`put` or `delete`), and for a put its `kind` and its blob or tree `id`.

```sh
so history main:src/lib.rs --human
```

```text
path: src/lib.rs
entries[0].changeset: ce8c4ae35795…
entries[0].op: put
entries[0].kind: file
entries[0].id: a15b2af67626…
entries[0].author_agent: agent/worker-1
entries[0].created_at: 1792626460000
entries[0].title: add g
entries[1].changeset: 3ba35c44081b…
entries[1].op: put
entries[1].kind: file
entries[1].id: f5f5220aa3d3…
entries[1].author_agent: agent/worker-1
entries[1].created_at: 1792626460000
entries[1].title: first
truncated: false
```

A directory's history gives the directory's tree after each change below it:

```sh
so history main:docs --human
```

```text
path: docs
entries[0].changeset: 48b8a4cf3ca9…
entries[0].op: put
entries[0].kind: dir
entries[0].id: cde13afef0d0…
entries[0].author_agent: agent/worker-1
entries[0].created_at: 1792626460000
entries[0].title: add a guide
truncated: false
```

A deletion is an entry too. Here `e8974b4bb892` is a changeset made later in this guide, which
deletes `README.md` (see [Undo an operation](#undo-an-operation)):

```sh
so history e8974b4bb892:README.md --human
```

```text
path: README.md
entries[0].changeset: e8974b4bb892…
entries[0].op: delete
entries[0].author_agent: agent/worker-1
entries[0].created_at: 1792626486000
entries[0].title: drop the readme
entries[1].changeset: 48b8a4cf3ca9…
entries[1].op: put
entries[1].kind: file
entries[1].id: a7a064662799…
entries[1].author_agent: agent/worker-1
entries[1].created_at: 1792626460000
entries[1].title: add a guide
entries[2].changeset: 3ba35c44081b…
entries[2].op: put
entries[2].kind: file
entries[2].id: 745ffc98a5d4…
entries[2].author_agent: agent/worker-1
entries[2].created_at: 1792626460000
entries[2].title: first
truncated: false
```

`-n N` (50 by default) and `--cursor` page it as they page `log`. A path no changeset in that
history touched is `E_PATH_NOT_FOUND` (exit 6):

```sh
so history main:nope --human
```

```text
error: E_PATH_NOT_FOUND: no changeset in the first-parent history of main touched "nope"
details.path: nope
details.rev: main
hint: swarmops ls main
```

## Compare two revisions

`so diff FROM TO` lists the files that differ between two revisions, in path order. Each has a
`status`: `added`, `deleted`, `modified` or `type_changed` (a symlink on one side, a file on the
other).

```sh
so diff main~2 main --human
```

```text
from: 3ba35c44081b…
to: 48b8a4cf3ca9…
files[0].path: README.md
files[0].status: modified
files[1].path: docs/guide.md
files[1].status: added
files[2].path: src/lib.rs
files[2].status: modified
truncated: false
```

### Say more of each file

`--format` picks how much: `names` (the default), `stat`, which adds whether each file is binary
and its inserted and deleted lines, and `unified`, which adds its kinds, its blobs and its hunks.

```sh
so diff main~2 main --format stat --human
```

```text
from: 3ba35c44081b…
to: 48b8a4cf3ca9…
files[0].path: README.md
files[0].status: modified
files[0].binary: false
files[0].insertions: 2
files[0].deletions: 0
files[1].path: docs/guide.md
files[1].status: added
files[1].binary: false
files[1].insertions: 1
files[1].deletions: 0
files[2].path: src/lib.rs
files[2].status: modified
files[2].binary: false
files[2].insertions: 4
files[2].deletions: 0
truncated: false
```

`--path P` keeps the files at or below `P`; give it again for more. `--context N` sets the
unchanged lines around each change in a hunk (3 by default):

```sh
so diff main~1 main --format unified --path README.md --human
```

```text
from: ce8c4ae35795…
to: 48b8a4cf3ca9…
files[0].path: README.md
files[0].status: modified
files[0].old_kind: file
files[0].new_kind: file
files[0].old_blob: 745ffc98a5d4…
files[0].new_blob: a7a064662799…
files[0].binary: false
files[0].insertions: 2
files[0].deletions: 0
files[0].hunks[0].old_start: 1
files[0].hunks[0].old_lines: 1
files[0].hunks[0].new_start: 1
files[0].hunks[0].new_lines: 3
files[0].hunks[0].lines[0].op: context
files[0].hunks[0].lines[0].text: |
  # acme
files[0].hunks[0].lines[1].op: add
files[0].hunks[0].lines[1].text: |
  
files[0].hunks[0].lines[2].op: add
files[0].hunks[0].lines[2].text: |
  See docs/guide.md.
truncated: false
```

A unified page holds at most `--max-bytes` of JSON (32 KiB by default); the first file comes
all the same, its hunks cut to fit.

### Page through a large diff

`--limit N` (500 by default) cuts the page, and `--cursor` reads on:

```sh
so diff main~2 main --limit 1 --human
```

```text
from: 3ba35c44081b…
to: 48b8a4cf3ca9…
files[0].path: README.md
files[0].status: modified
truncated: true
cursor: README.md
```

```sh
so diff main~2 main --limit 1 --cursor README.md --human
```

```text
from: 3ba35c44081b…
to: 48b8a4cf3ca9…
files[0].path: docs/guide.md
files[0].status: added
truncated: true
cursor: docs/guide.md
```

### See what one changeset changed

Diff the changeset against its first parent, `X~1`:

```sh
so diff e8974b4bb892~1 e8974b4bb892 --human
```

```text
from: 48b8a4cf3ca9…
to: e8974b4bb892…
files[0].path: README.md
files[0].status: deleted
truncated: false
```

The first changeset of a history has no parent to diff against; `so show` lists its changes.

## List the refs

`so ref list` gives every ref, its target and its `version`: how many committed moves changed
its target.

```sh
so ref set release/1.0 main~1 --create --human
```

```text
name: release/1.0
new: ce8c4ae35795…
already_applied: false
```

```sh
so ref list --human
```

```text
refs[0].name: main
refs[0].target: 48b8a4cf3ca9…
refs[0].version: 3
refs[1].name: release/1.0
refs[1].target: ce8c4ae35795…
refs[1].version: 1
```

A ref is `main` or `release/<name>`.

## See every write

`so op log` lists the store's operations, newest first. An operation is one write transaction:
a frame of the store's journal, numbered from 0 in commit order. Each gives its `kind`, the
changesets it recorded and the ref moves it made, with who made each move and when (`at`, in
Unix seconds).

`main` moves only forward: `ref set` refuses to move it off its first-parent history, with
`E_REF_PROTECTED` (exit 1), and writes nothing. `details.changeset` is the head the move would
drop:

```sh
so ref set main main~2 --expect 48b8a4cf3ca95a01caec61568a22e7f4f0ad89ef71b7d1c9ac85e57544b3b641 --human
```

```text
error: E_REF_PROTECTED: main moves only forward: 48b8a4cf3ca95a01caec61568a22e7f4f0ad89ef71b7d1c9ac85e57544b3b641 is not a first-parent ancestor of 3ba35c44081b8f1773b4e013f0fd3b7af5f59d67d9faa6e0d42aad4016916cfb, so nothing was written
details.changeset: 48b8a4cf3ca95a01caec61568a22e7f4f0ad89ef71b7d1c9ac85e57544b3b641
details.name: main
details.target: 3ba35c44081b8f1773b4e013f0fd3b7af5f59d67d9faa6e0d42aad4016916cfb
hint: main moves only forward: land a change that takes it back
```

A release ref moves either way. Here an agent moves `release/1.0` back one changeset by mistake,
and the op log shows it as operation 4:

```sh
so ref set release/1.0 main~2 --expect ce8c4ae3579542fb110b8bec045e1b1fb0655de9389d7a2256b72e16daabb8cb --human
```

```text
name: release/1.0
old: ce8c4ae35795…
new: 3ba35c44081b…
already_applied: false
```

```sh
so op log -n 2 --human
```

```text
ops[0].op: 4
ops[0].id: 4:3d1f09a2
ops[0].kind: ref_move
ops[0].changesets: []
ops[0].changesets_total: 0
ops[0].ref_moves[0].name: release/1.0
ops[0].ref_moves[0].old: ce8c4ae35795…
ops[0].ref_moves[0].new: 3ba35c44081b…
ops[0].ref_moves[0].actor: agent/worker-1
ops[0].ref_moves[0].at: 1792626486
ops[1].op: 3
ops[1].id: 3:0c4f8a21
ops[1].kind: ref_move
ops[1].changesets: []
ops[1].changesets_total: 0
ops[1].ref_moves[0].name: release/1.0
ops[1].ref_moves[0].new: ce8c4ae35795…
ops[1].ref_moves[0].actor: agent/worker-1
ops[1].ref_moves[0].at: 1792626486
truncated: true
```

A move without `old` created the ref; one without `new` deleted it. The kinds are:

| Kind | The operation |
| --- | --- |
| `commit` | Recorded changesets and moved no ref (`commit` without `--advance`). |
| `publish` | Recorded one changeset and moved a ref to it (`commit --advance`). |
| `land` | Recorded one changeset that names a lander and moved a ref to it (`land`). |
| `ref_move` | Moved refs and recorded no changeset (`ref set`, `ref delete`, `op undo`). |
| `compaction` | Recorded a compaction of the changeset index, and nothing else. |
| `mixed` | Any other mix of changesets, ref moves and compactions. |

`-n N` (50 by default) cuts the page. A cut page says `truncated: true`; pass the last `op` it
listed as `--before` for the next one:

```sh
so op log --before 3 -n 2 --human
```

```text
ops[0].op: 2
ops[0].id: 2:b27e94c5
ops[0].kind: publish
ops[0].changesets[0].id: 48b8a4cf3ca9…
ops[0].changesets[0].agent: agent/worker-1
ops[0].changesets[0].created_at: 1792626460000
ops[0].changesets[0].title: add a guide
ops[0].changesets_total: 1
ops[0].ref_moves[0].name: main
ops[0].ref_moves[0].old: ce8c4ae35795…
ops[0].ref_moves[0].new: 48b8a4cf3ca9…
ops[0].ref_moves[0].actor: agent/worker-1
ops[0].ref_moves[0].at: 1792626460
ops[1].op: 1
ops[1].id: 1:6a1d0f3e
ops[1].kind: publish
ops[1].changesets[0].id: ce8c4ae35795…
ops[1].changesets[0].agent: agent/worker-1
ops[1].changesets[0].created_at: 1792626460000
ops[1].changesets[0].title: add g
ops[1].changesets_total: 1
ops[1].ref_moves[0].name: main
ops[1].ref_moves[0].old: 3ba35c44081b…
ops[1].ref_moves[0].new: ce8c4ae35795…
ops[1].ref_moves[0].actor: agent/worker-1
ops[1].ref_moves[0].at: 1792626460
truncated: true
```

`--max-changesets N` (20 by default) caps the changesets listed for each operation;
`changesets_total` still counts them all.

## Undo an operation

`so op undo ID` moves every ref the operation `ID` moved back to what it named before, in one
transaction: all of them or none. `ID` is the operation's `id` in `op log`, `<op>:<crc>`: its number
and the checksum of what it recorded, so an id names one operation and no other. It is a write, so
it needs an identity. It never removes a changeset: what the ref named stays in the store, readable
by id. An undo moves `main` only forward too: an undo that would take it back is refused (see
[What an undo refuses](#what-an-undo-refuses)).

### Take back a mistaken ref move

Operation 4 moved `release/1.0` from `ce8c4ae35795…` to `3ba35c44081b…`. Undo it:

```sh
so op undo 4:3d1f09a2 --human
```

```text
undone: 4
op: 5
id: 5:e8a3b617
ref_moves[0].name: release/1.0
ref_moves[0].old: 3ba35c44081b…
ref_moves[0].new: ce8c4ae35795…
ref_moves[0].actor: agent/worker-1
ref_moves[0].at: 1792626486
already_applied: false
```

`release/1.0` names `ce8c4ae35795…` again. The undo is an operation of its own, operation 5.

### Run an undo again

An undo whose refs already name what they named before writes nothing, and says
`already_applied: true`. Retrying an undo is safe:

```sh
so op undo 4:3d1f09a2 --human
```

```text
undone: 4
ref_moves: []
already_applied: true
```

### Undo an undo

An undo is an operation like any other, so another undo takes it back. Undoing operation 5
moves `release/1.0` to `3ba35c44081b…` again:

```sh
so op undo 5:e8a3b617 --human
```

```text
undone: 5
op: 6
id: 6:47c2d9e0
ref_moves[0].name: release/1.0
ref_moves[0].old: ce8c4ae35795…
ref_moves[0].new: 3ba35c44081b…
ref_moves[0].actor: agent/worker-1
ref_moves[0].at: 1792626486
already_applied: false
```

### When a ref moved since

Each ref must still name what the operation set it to. Here an agent publishes on
`release/1.0` (operation 7), and then operation 6 is undone:

```sh
so commit -m 'add notes' --changes c4.json --advance release/1.0 --human
```

```text
changeset: e1ff8dd6a805…
parent: 3ba35c44081b…
root: 56bce18a5de6…
files_changed: 1
blobs_written: 1
advanced.name: release/1.0
advanced.old: 3ba35c44081b…
advanced.new: e1ff8dd6a805…
advanced.already_applied: false
replayed: false
```

`c4.json` puts one file, `NOTES.md`. `release/1.0` no longer names what operation 6 set it to, so the
undo refuses with `E_CAS_MISMATCH` (exit 4) and writes nothing. `details.actual` is what the ref
names now:

```sh
so op undo 6:47c2d9e0 --human
```

```text
error: E_CAS_MISMATCH: ref release/1.0 has moved since operation 6 set it to 3ba35c44081b…: it names e1ff8dd6a805… now, so this undo wrote nothing; undo the operations that moved it since, newest first, then this one again, or land a change that reverses operation 6
details.actual: e1ff8dd6a805…
details.name: release/1.0
details.op: 6
retryable: true
hint: swarmops op log
```

The message says the two ways on: undo the later moves first, or land a change.
[Recover from a refusal](#recover-from-a-refusal) follows the first, after the next examples.

### What an undo refuses

An operation that moved no ref has nothing to undo. Operation 8 was a commit without
`--advance`, the one that deleted `README.md`:

```sh
so commit -m 'drop the readme' --changes c5.json --parent main --human
```

```text
changeset: e8974b4bb892…
parent: 48b8a4cf3ca9…
root: 9b9529c18264…
files_changed: 1
blobs_written: 0
replayed: false
```

```sh
so op undo 8:f1a6720c --human
```

```text
error: E_NOTHING_TO_UNDO: operation 8 moved no ref, so there is nothing to undo
details.op: 8
hint: swarmops op log
```

An undo that would move `main` off its first-parent history is `E_REF_PROTECTED` (exit 1), and
writes nothing. Operation 2 moved `main` forward to `48b8a4cf3ca9…`, so its undo would take
`main` back; `details.op` and `details.id` name the operation:

```sh
so op undo 2:b27e94c5 --human
```

```text
error: E_REF_PROTECTED: main moves only forward: 48b8a4cf3ca95a01caec61568a22e7f4f0ad89ef71b7d1c9ac85e57544b3b641 is not a first-parent ancestor of ce8c4ae3579542fb110b8bec045e1b1fb0655de9389d7a2256b72e16daabb8cb (the undo of operation 2:b27e94c5), so nothing was written
details.changeset: 48b8a4cf3ca95a01caec61568a22e7f4f0ad89ef71b7d1c9ac85e57544b3b641
details.id: 2:b27e94c5
details.name: main
details.op: 2
details.target: ce8c4ae3579542fb110b8bec045e1b1fb0655de9389d7a2256b72e16daabb8cb
hint: main moves only forward: land a change that takes it back
```

To take `main` back, land a change that reverses what the operation did.

An id whose number is past the last operation is `E_OP_NOT_FOUND` (exit 6), and so is one whose
checksum is not that operation's (`details.reason: checksum`):

```sh
so op undo 99:00000000 --human
```

```text
error: E_OP_NOT_FOUND: there is no operation 99:00000000: its number is past the last operation (the last is 8)
details.id: 99:00000000
details.last: 8
details.op: 99
details.reason: past_end
hint: swarmops op log
```

A bare number is no id, and is `E_INVALID_ARG` (exit 2):

```sh
so op undo 4 --human
```

```text
error: E_INVALID_ARG: not an operation id: expected <op>:<crc>, as `op log` prints it (4:9f3a01c2)
details.id: 4
hint: swarmops op log
```

Without an identity, an undo is `E_IDENTITY_REQUIRED` (exit 5). An empty variable counts as
unset:

```sh
SWARMOPS_AGENT= so op undo 4:3d1f09a2 --human
```

```text
error: E_IDENTITY_REQUIRED: this operation changes the store, so it needs the caller's identity
hint: pass --as <family>/<instance> or set SWARMOPS_AGENT, e.g. --as agent/worker-1
```

### Undo a ref's deletion

An undo of a deletion creates the ref again. Operation 9 deleted `release/1.0`:

```sh
so ref delete release/1.0 --expect e1ff8dd6a8059696005d423d858775dc83b2618a2dc104030fdfbfd6afb2f541 --human
```

```text
name: release/1.0
old: e1ff8dd6a805…
already_applied: false
```

```sh
so op undo 9:5e8d1b39 --human
```

```text
undone: 9
op: 10
id: 10:a2c7f480
ref_moves[0].name: release/1.0
ref_moves[0].new: e1ff8dd6a805…
ref_moves[0].actor: agent/worker-1
ref_moves[0].at: 1792626486
already_applied: false
```

### Recover from a refusal

Back to the refusal of operation 6: `release/1.0` moved since, by operation 7. Undo the later
move first, newest first, then operation 6:

```sh
so op undo 7:9b05e3f4 --human
```

```text
undone: 7
op: 11
id: 11:1f93b6d2
ref_moves[0].name: release/1.0
ref_moves[0].old: e1ff8dd6a805…
ref_moves[0].new: 3ba35c44081b…
ref_moves[0].actor: agent/worker-1
ref_moves[0].at: 1792626544
already_applied: false
```

```sh
so op undo 6:47c2d9e0 --human
```

```text
undone: 6
op: 12
id: 12:c6e05a17
ref_moves[0].name: release/1.0
ref_moves[0].old: 3ba35c44081b…
ref_moves[0].new: ce8c4ae35795…
ref_moves[0].actor: agent/worker-1
ref_moves[0].at: 1792626544
already_applied: false
```

That takes `release/1.0` off the changeset operation 7 published, and the changeset stays in the
store:

```sh
so cat e1ff8dd6a805:NOTES.md --human
```

```text
blob: 8a45e3b185d6…
size: 15
kind: file
encoding: utf8
content: |
  Release notes.
truncated: false
```

When the later moves must stay, land a change that reverses what the operation did instead.

### Undo a ref's creation

An undo of a creation deletes the ref. Operation 3 created `release/1.0`:

```sh
so op undo 3:0c4f8a21 --human
```

```text
undone: 3
op: 13
id: 13:7d4a28bf
ref_moves[0].name: release/1.0
ref_moves[0].old: ce8c4ae35795…
ref_moves[0].actor: agent/worker-1
ref_moves[0].at: 1792626544
already_applied: false
```

```sh
so ref list --human
```

```text
refs[0].name: main
refs[0].target: 48b8a4cf3ca9…
refs[0].version: 3
```

## Check the store

`so verify` checks the structure: parents, that every object a changeset references exists, the
index against the journal, and the refs. `so verify --deep` also re-hashes every object, and
checks the derived values against the objects. A clean store succeeds:

```sh
so verify --human
```

```text
objects_checked: 19
changesets_checked: 5
problems: []
findings: []
```

A store with damage fails with `E_STORE_CORRUPT` (exit 1). `details.problems` gives one line
for each problem, and `details.findings` the same problems for a program, by kind and id. On a
copy of the store, `broken`, with one bit of one object file flipped, the structural check
passes and the deep one finds the damage:

```sh
so --repo broken verify --human
```

```text
objects_checked: 19
changesets_checked: 5
problems: []
findings: []
```

```sh
so --repo broken verify --deep --human
```

```text
error: E_STORE_CORRUPT: verify found a problem: object a7a064662799… is corrupt
details.changesets_checked: 5
details.findings[0].kind: corrupt_object
details.findings[0].object: a7a064662799…
details.objects_checked: 19
details.problems[0]: object a7a064662799… is corrupt
hint: details.findings names each damaged object, record or ref by kind and id: restore them from a backup of the store
```

A read that meets the damaged object fails too, because every object read is checked against
its id:

```sh
so --repo broken cat 48b8a4cf3ca9:README.md --human
```

```text
error: E_STORE_CORRUPT: object a7a064662799… is corrupt: its content does not match its id
details.object: a7a064662799…
hint: swarmops verify --deep
```

There is no repair tool yet. [Checking a store](../operations.md#checking-a-store) says when to
run `verify --deep`, and [Backup and restore](../operations.md#backup-and-restore) how to
restore.

## Choose an output format

Every command here prints one JSON envelope on stdout. Three flags, given before or after the
subcommand, choose how:

| Flag | Prints |
| --- | --- |
| `--json` | The envelope as JSON: the default. |
| `--pretty[=BOOL]` | The JSON indented. The default is pretty when stdout is a terminal, compact otherwise. |
| `--human` | A short text rendering instead of JSON. |

Without `--json` or `--human`, `SWARMOPS_FORMAT` decides: `json` or `human`. Of `--json` and
`--human`, the one given last wins. The exit code is the same in every format. When the
command line itself is invalid, a format flag after the bad value can go unread:
`so log -n many --human` prints JSON.

Compact JSON, as a script reads it:

```sh
so resolve main~1
```

```text
{"ok":true,"op":"resolve_rev","api":1,"result":{"id":"ce8c4ae35795…","kind":"changeset"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{"main~1":"ce8c4ae35795…"},"auth":"trust"}}
```

Pretty JSON:

```sh
so resolve main~1 --pretty
```

```text
{
  "ok": true,
  "op": "resolve_rev",
  "api": 1,
  "result": {
    "id": "ce8c4ae35795…",
    "kind": "changeset"
  },
  "meta": {
    "repo": "acme",
    "elapsed_ms": 0,
    "resolved": {
      "main~1": "ce8c4ae35795…"
    },
    "auth": "trust"
  }
}
```

Text, from the variable, and `--json` overriding it:

```sh
SWARMOPS_FORMAT=human so resolve main~1
```

```text
id: ce8c4ae35795…
kind: changeset
```

```sh
SWARMOPS_FORMAT=human so resolve main~1 --json
```

```text
{"ok":true,"op":"resolve_rev","api":1,"result":{"id":"ce8c4ae35795…","kind":"changeset"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{"main~1":"ce8c4ae35795…"},"auth":"trust"}}
```

Any other value of the variable is `E_INVALID_ARG` (exit 2), said of the variable:

```sh
SWARMOPS_FORMAT=yaml so resolve main~1
```

```text
{"ok":false,"op":"resolve_rev","api":1,"error":{"code":"E_INVALID_ARG","message":"SWARMOPS_FORMAT: \"yaml\" is not a format: json or human","retryable":false,"exit":2,"details":{"source":"SWARMOPS_FORMAT"},"hint":"set SWARMOPS_FORMAT to json or human, or unset it"},"meta":{"elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

### Read the envelope

The JSON keeps one order: `ok`, `op`, `api`, then `result` on success or `error` on failure,
never both, then `meta`.

- `op` is the operation's name, which is not always the command's: `cat` is `read_file`. The
  table below lists them.
- `api` is the API version, 1.
- `meta.repo` is the store as you named it, `meta.elapsed_ms` how long the command took,
  `meta.resolved` each revision it resolved with the full id it named, and `meta.auth` the
  store's authentication mode, `trust`.
- `error` carries a stable `code`, a `message`, `retryable` (whether running it again can
  succeed), `exit`, machine-readable `details` and a `hint`, often the next command to run.

A failure as JSON, a command line the program refuses:

```sh
so log -n many
```

```text
{"ok":false,"op":"log","api":1,"error":{"code":"E_INVALID_ARG","message":"invalid value 'many' for '--limit <N>': invalid digit found in string","retryable":false,"exit":2,"details":{},"hint":"Usage: so log [OPTIONS] [REV]\nFor more information, try '--help'."},"meta":{"elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

`--human` prints a result as `key: value` lines in the same order: a nested key as
`outer.inner`, an array item as `key[i]`, an empty array as `[]`, and a multi-line string as
`key: |` with its lines indented by two spaces. A failure prints `error: <code>: <message>`,
then its details as `details.<key>` lines, `retryable: true` when it is, and the hint. The text
is for a person to read; a program reads the JSON.

Of the commands here, only `--help`, `--version` and the `help` subcommand print plain text. A run that meets one of
the store's four documented aborts prints no envelope: the process aborts, saying why on stderr.

### Commands and their operation names

| Command | `op` | MCP tool | Needs an identity |
| --- | --- | --- | --- |
| `resolve` | `resolve_rev` | `resolve_rev` | no |
| `cat` | `read_file` | `read_file` | no |
| `ls` | `read_tree` | `read_tree` | no |
| `show` | `show` | `show` | no |
| `log` | `log` | `log` | no |
| `history` | `path_history` | `path_history` | no |
| `diff` | `diff` | `diff` | no |
| `ref list` | `ref_list` | `ref_list` | no |
| `ref set` | `ref_set` | `ref_set` | yes |
| `ref delete` | `ref_delete` | `ref_delete` | yes |
| `op log` | `op_log` | `op_log` | no |
| `op undo` | `undo` | `undo` | yes |
| `verify` | `verify` | `verify` | no |

An agent on MCP calls the tool of the same name with the operation's request, and gets the same
envelope back.

## Exit codes

The exit code is the envelope's `error.exit`, and 0 on success.

| Exit | Meaning | From the commands in this guide |
| --- | --- | --- |
| 0 | Success, including an undo that was already applied. | |
| 1 | The operation failed. | `E_IS_DIR`, `E_AMBIGUOUS_ID`, `E_NOTHING_TO_UNDO`, `E_REF_PROTECTED`, `E_STORE_CORRUPT` |
| 2 | A malformed request or command line. | `E_INVALID_ARG`: a bad revision, cursor, flag value or `SWARMOPS_FORMAT` |
| 3 | A conflict with what a change lands on. | None here: `land`, `git import` and `git export` raise it. |
| 4 | Retryable: running it again can succeed, after re-reading what the error names. | `E_CAS_MISMATCH` |
| 5 | An identity or policy requirement refused it. | `E_IDENTITY_REQUIRED` |
| 6 | Something it names does not exist. | `E_REV_NOT_FOUND`, `E_PATH_NOT_FOUND`, `E_OP_NOT_FOUND`, `E_NOT_A_REPO` |

`so schema` gives every request and response type, and the README's
[Output and exit codes](../../README.md#output-and-exit-codes) lists the codes of every command.
