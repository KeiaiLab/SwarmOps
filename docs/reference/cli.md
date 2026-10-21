<!-- Generated from the code by crates/swarmops-cli/src/reference.rs: do not edit. Change the code, then run SWARMOPS_REGEN_REFERENCE=1 with the regen_reference test. -->

# CLI reference

Every command of `swarmops` and `so`, its short form: the same program by a shorter name. This page is generated from the command line's own definition, so it says what `swarmops <command> --help` says. `-h` prints the short help and `--help` the long help that this page gives.

Every command but `mcp` prints one JSON envelope on stdout; its `op` names the operation the command runs (`cat` runs `read_file`). [Errors](errors.md) lists every error code an envelope can carry, with its exit code.

## Commands

| Command | Operation | What it does |
| --- | --- | --- |
| [`init`](#swarmops-init) | `repo_init` | Create a store in DIR, and describe it. |
| [`info`](#swarmops-info) | `repo_info` | Describe the store. |
| [`whoami`](#swarmops-whoami) | `whoami` | Show the identity this caller's changes carry. |
| [`schema`](#swarmops-schema) | `schema` | Print the JSON Schema of every request and response type, or of TYPE's. |
| [`commit`](#swarmops-commit) | `commit` | Record a changeset from a list of changes; with --advance, also move that ref to it. |
| [`land`](#swarmops-land) | `land` | Merge a change three-way onto a ref and publish it there as one new changeset. |
| [`blob put`](#swarmops-blob-put) | `blob_put` | Store FILE's content as a blob, outside any commit, and print its id, which a change names it by: {"blob": ID}. A change list holds at most 64 MiB and a commit's contents 1 GiB together; a blob named by id counts toward neither. |
| [`blob has`](#swarmops-blob-has) | `blob_has` | Of the IDs given, list those the store holds blobs under. |
| [`resolve`](#swarmops-resolve) | `resolve_rev` | Name the changeset REV names, and whether REV is a ref's name. |
| [`cat`](#swarmops-cat) | `read_file` | Print one file at a revision, or part of it: from a byte, or a range of lines. |
| [`ls`](#swarmops-ls) | `read_tree` | List one directory at a revision (the root without a PATH), and with --depth the directories below it. |
| [`show`](#swarmops-show) | `show` | Show one changeset. |
| [`log`](#swarmops-log) | `log` | List history along first parents, newest first. |
| [`history`](#swarmops-history) | `path_history` | List the changesets that changed a path, newest first, along first parents. |
| [`diff`](#swarmops-diff) | `diff` | List the files that differ between two revisions, a page at a time. |
| [`ref list`](#swarmops-ref-list) | `ref_list` | List every ref. |
| [`ref set`](#swarmops-ref-set) | `ref_set` | Point ref NAME at REV: move it from --expect, or --create it. |
| [`ref delete`](#swarmops-ref-delete) | `ref_delete` | Delete ref NAME. |
| [`op log`](#swarmops-op-log) | `op_log` | List the store's operations, newest first: each write transaction, its changesets and its ref moves. |
| [`op undo`](#swarmops-op-undo) | `undo` | Move every ref operation OP moved back to what it named before, all or none. |
| [`verify`](#swarmops-verify) | `verify` | Check the store's consistency. |
| [`git import`](#swarmops-git-import) | `git_import` | Bring a git repository's history in: every commit --ref reaches that the store does not hold, then move --into to it. |
| [`git export`](#swarmops-git-export) | `git_export` | Send the store's history to git: write every commit of --ref's history that --branch's last export does not reach into the git repository at PATH, then move --branch to it. |
| [`mcp`](#swarmops-mcp) | `mcp` | Serve the store's operations to an agent as an MCP server on stdin and stdout. |
| [`derive`](#swarmops-derive) | `derive` | Derive the eager derived values of new changesets, ahead of any reader. |

## Global options

Every command takes these, before or after its subcommand.

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `--repo <STORE>` | no | The store to use (default: $SWARMOPS\_REPO). |  |
| `--as <AGENT>` | no | The agent to act as, &lt;family&gt;/&lt;instance&gt; (default: $SWARMOPS\_AGENT). |  |
| `--json` | no | Print the JSON envelope: the default, unless $SWARMOPS\_FORMAT is human. |  |
| `--human` | no | Print a short text rendering instead of JSON. |  |
| `--pretty[=<BOOL>]` | no | Pretty-print the JSON (default: when stdout is a terminal). |  |

`-h` prints a command's short help and `--help` its long help, as plain text, not an envelope. `swarmops --version` prints the version.

## swarmops init

Create a store in DIR, and describe it.

Envelope `op`: `repo_init`.

```text
swarmops init [OPTIONS] <DIR>
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `DIR` | yes | The directory to create the store in. |  |

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `--name <NAME>` | no | The repository's name, at most 255 bytes (default: the directory's name). |  |

Example: [Operating SwarmOps: Create a store](../operations.md#create-a-store).

## swarmops info

Describe the store.

Envelope `op`: `repo_info`.

```text
swarmops info [OPTIONS]
```

## swarmops whoami

Show the identity this caller's changes carry.

An identified caller gets its agent, its run id when it has one, and auth "trust"; a caller without an identity gets auth "anonymous" and no agent field at all.

Envelope `op`: `whoami`.

```text
swarmops whoami [OPTIONS]
```

Example: [Operating SwarmOps: Identities](../operations.md#identities).

## swarmops schema

Print the JSON Schema of every request and response type, or of TYPE's.

Envelope `op`: `schema`.

```text
swarmops schema [OPTIONS] [TYPE]
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `TYPE` | no | One type's name, such as CommitReq. |  |

## swarmops commit

Record a changeset from a list of changes; with --advance, also move that ref to it.

Envelope `op`: `commit`.

```text
swarmops commit [OPTIONS] --message <MESSAGE> --changes <FILE>
```

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `-m, --message <MESSAGE>` | yes | The commit message. |  |
| `--changes <FILE>` | yes | A JSON file holding an array of changes, or - for stdin: at most 64 MiB. |  |
| `--parent <REV>` | no | The changeset to commit on top of (default: main's head, if main exists). |  |
| `--allow-empty` | no | Record the changeset even if it changes nothing. |  |
| `--advance <REF>` | no | Also move REF to the new changeset, in the same transaction, if REF still names --parent. |  |
| `--key <UUID>` | no | A uuid v7 that makes a retry safe: the same commit run again replays (replayed: true). |  |

`--advance <REF>`:

Also move REF (main or release/&lt;name&gt;) to the new changeset, in the same transaction: both are recorded, or neither.

The move happens only if REF still names --parent (for a root commit, only if REF does not exist yet), so it is always a one-step fast-forward. Give --parent as the full changeset id you read REF at (from ref list, show, log or a commit's result): a ref name resolves when the command runs, and without --parent the commit builds on REF's head as it is then, so the move catches only a race from that moment on.

After E\_CAS\_MISMATCH nothing was recorded, neither the changeset nor the move: commit your change without --advance and land it (swarmops land merges it onto REF's head), or re-apply your edits to the content at the error's `actual` and commit with --parent &lt;actual&gt; --advance REF. Do not re-send your old whole-file puts: they would revert the winner's edits to shared files.

`--key <UUID>`:

A uuid v7 (RFC 9562) that makes a retry of this commit safe: make a new one for each commit, and give it again only to retry that commit.

The commit signs at the key's time, to the second, and records the key as its commit\_key metadata, so the same command run again on the same parent makes the same changeset, which the store holds already: the result says replayed: true, with the first run's changeset, and nothing is written. A commit whose parent is the changeset its key made (without --parent, main's head, or REF's with --advance) replays that changeset instead of committing it again on top of itself.

Give --parent as the full changeset id you read: without it the commit builds on main's head (REF's, with --advance) as it is when the command runs, so a retry after that head moved to another changeset commits again, on the new head.

Example: [Operating SwarmOps: Agents commit and land](../operations.md#2-agents-commit-and-land).

## swarmops land

Merge a change three-way onto a ref and publish it there as one new changeset.

The change is HEAD with its first-parent chain since the base, squashed: the new changeset's parent is the ref's head, it holds what the chain changed merged with what the ref gained since the base, and the ref moves to it in the same transaction. A ref that moves meanwhile makes the land merge again onto its new head and retry, up to --max-attempts times. A conflict is E\_CONFLICT (exit 3), with each conflicting file in details.report, and nothing is recorded; land again with --resolve to resolve it without a checkout.

Envelope `op`: `land`.

```text
swarmops land [OPTIONS] <HEAD>
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `HEAD` | yes | The changeset to land: the change's head, such as the changeset a commit made. |  |

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `--base <REV>` | no | Where the change starts: a first-parent ancestor of HEAD, whose own changes are not landed (default: the merge base of HEAD and the ref's head, along first parents). |  |
| `--target <REF>` | no | The ref to land on: main or release/&lt;name&gt;. | `main` |
| `-m, --message <MSG>` | no | The new changeset's message (default: HEAD's). |  |
| `--key <KEY>` | no | Makes a retry safe: the same land run again replays it (status: replayed). |  |
| `--max-attempts <N>` | no | How many times to try to publish before E\_CAS\_RETRY\_EXHAUSTED: 1 to 32. | `8` |
| `--max-conflict-bytes <N>` | no | The most bytes of hunk text an E\_CONFLICT report carries; past them it leaves the texts out (truncated) and keeps every line position and blob id. | `32768` |
| `--resolve <FILE>` | no | Resolve the conflicts a report named: resolutions as JSON in FILE, or - for stdin. |  |

`--key <KEY>`:

Makes a retry of this land safe: 1 to 64 letters, digits and '-', such as a uuid v7. Make a new one for each change you land, and give it again only to retry that land.

A land with a key first looks down the ref's first parents, to the change's base, for a changeset landed with the same key: one squashed from this HEAD is the land made before, which the land replays (status: replayed) without writing anything; one from another head is E\_INVALID\_ARG. Without a key, the same land run again finds its change on the ref already: E\_NOTHING\_TO\_LAND.

`--resolve <FILE>`:

Resolve the conflicts an E\_CONFLICT report named, without a checkout: resolutions as a JSON object in FILE, or - for stdin, at most 64 MiB (swarmops schema ResolutionsInput).

{"against": THEIRS, "files": {PATH: RESOLUTION, ...}}: THEIRS is the report's theirs, by its full id, and each RESOLUTION one of {"take": "ours"}, {"take": "theirs"}, {"content": CONTENT} with an optional "mode" (CONTENT as a commit's change gives it: inline, base64, a file, or a stored blob by id) and {"delete": true}. A resolution decides its path and every path below it; the rest of the merge stands.

A resolution applies while the ref holds its path as THEIRS did, whether the path conflicts or not. A path the ref changed since is stale: the land fails with E\_CONFLICT again, its report naming the path in stale, as it does for each conflict left unresolved. Read the report's theirs, and resolve against it.

Example: [Operating SwarmOps: Agents commit and land](../operations.md#2-agents-commit-and-land).

## swarmops blob put

Store FILE's content as a blob, outside any commit, and print its id, which a change names it by: {"blob": ID}. A change list holds at most 64 MiB and a commit's contents 1 GiB together; a blob named by id counts toward neither.

Envelope `op`: `blob_put`.

```text
swarmops blob put [OPTIONS] <FILE>
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `FILE` | yes | The file whose content to store, or - for stdin. |  |

## swarmops blob has

Of the IDs given, list those the store holds blobs under.

Envelope `op`: `blob_has`.

```text
swarmops blob has [OPTIONS] <ID>...
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `ID` | yes | Blob ids, 64 hex digits each. |  |

## swarmops resolve

Name the changeset REV names, and whether REV is a ref's name.

Envelope `op`: `resolve_rev`.

```text
swarmops resolve [OPTIONS] <REV>
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `REV` | yes | The revision: a ref name, a full changeset id or a unique prefix of 12 or more of its hex digits, then any number of \~N suffixes. |  |

Example: [Operating SwarmOps: One writer per trunk](../operations.md#one-writer-per-trunk).

## swarmops cat

Print one file at a revision, or part of it: from a byte, or a range of lines.

Envelope `op`: `read_file`.

```text
swarmops cat [OPTIONS] <REV:PATH>
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `REV:PATH` | yes | The revision and the file's path, split at the first ':'. |  |

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `--offset <N>` | no | Start at byte N of the file; a truncated result's next\_offset reads on from there. | `0` |
| `--lines <A-B>` | no | Print lines A to B only, numbered from 1, both included, such as 10-20. |  |
| `--max-bytes <N>` | no | The most content bytes to print. | `1048576` |

Example: [README: First commands](../../README.md#first-commands).

## swarmops ls

List one directory at a revision (the root without a PATH), and with --depth the directories below it.

Envelope `op`: `read_tree`.

```text
swarmops ls [OPTIONS] <REV[:PATH]>
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `REV[:PATH]` | yes | The revision, and the directory's path after the first ':'. |  |

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `--depth <N>` | no | How many levels to list: 1 for the directory's own entries, 2 for theirs too, and so on, up to 64. | `1` |
| `--limit <N>` | no | The most entries to list. | `250` |
| `--cursor <CURSOR>` | no | Resume a listing: the previous page's cursor, exactly as it was returned. |  |

Example: [README: First commands](../../README.md#first-commands).

## swarmops show

Show one changeset.

Envelope `op`: `show`.

```text
swarmops show [OPTIONS] <REV>
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `REV` | yes | The changeset. |  |

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `--max-changes <N>` | no | The most changes to list. | `400` |

Example: [README: First commands](../../README.md#first-commands).

## swarmops log

List history along first parents, newest first.

Envelope `op`: `log`.

```text
swarmops log [OPTIONS] [REV]
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `REV` | no | Where to start (default: main). |  |

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `-n, --limit <N>` | no | The most changesets to list. | `50` |
| `--cursor <CURSOR>` | no | Resume a log: the previous page's cursor, exactly as it was returned. |  |
| `--path <PATH>...` | no | Only the changesets that change PATH, or anything below it; give it again for more. |  |

Example: [Operating SwarmOps: Look at what landed](../operations.md#3-look-at-what-landed).

## swarmops history

List the changesets that changed a path, newest first, along first parents.

Envelope `op`: `path_history`.

```text
swarmops history [OPTIONS] <REV:PATH>
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `REV:PATH` | yes | The revision and the path (a file, or a directory), split at the first ':'. |  |

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `-n, --limit <N>` | no | The most changesets to list. | `50` |
| `--cursor <CURSOR>` | no | Resume a history: the previous page's cursor, exactly as it was returned. |  |

Example: [Operating SwarmOps: Making room](../operations.md#making-room).

## swarmops diff

List the files that differ between two revisions, a page at a time.

Envelope `op`: `diff`.

```text
swarmops diff [OPTIONS] <FROM> <TO>
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `FROM` | yes | The revision to diff from. |  |
| `TO` | yes | The revision to diff to. |  |

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `--path <P>...` | no | Only the files at or below this path; give it again for more. |  |
| `--format <FORMAT>` | no | What to say of each file. Values: `names`: Its path and status; `stat`: Also whether it is binary and, for text, the lines it inserts and deletes; `unified`: Also its kinds, its blobs and, for text, its hunks. | `names` |
| `--context <N>` | no | The unchanged lines around each change in a unified hunk. | `3` |
| `--limit <N>` | no | The most files to list. | `500` |
| `--cursor <CURSOR>` | no | Resume a diff: the previous page's cursor, exactly as it was returned. |  |
| `--max-bytes <N>` | no | The most bytes of a unified page's files, as JSON (the first file comes all the same, its hunks cut to fit). | `32768` |

Example: [Operating SwarmOps: Look at what landed](../operations.md#3-look-at-what-landed).

## swarmops ref list

List every ref.

Envelope `op`: `ref_list`.

```text
swarmops ref list [OPTIONS]
```

## swarmops ref set

Point ref NAME at REV: move it from --expect, or --create it.

Envelope `op`: `ref_set`.

```text
swarmops ref set [OPTIONS] <--expect <REV>|--create> <NAME> <REV>
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `NAME` | yes | main or release/&lt;name&gt;. |  |
| `REV` | yes | The revision to point it at. |  |

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `--expect <REV>` | no | The changeset the ref holds now: a full id, not a ref name. |  |
| `--create` | no | Create the ref; if it holds REV already that is a replay (already\_applied), at another target E\_CAS\_MISMATCH. |  |

Exactly one of `--expect <REV>` or `--create` is required.

`--expect <REV>`:

The changeset the ref holds now.

Give the full changeset id you read (from ref list, show, log or a commit's result). A ref name or a relative revision such as main\~1 is read from the store when the command runs instead of pinning the id you read: the ref's own name always matches (a deleted ref errors instead), so the compare-and-swap protects nothing, and main\~1 on main never matches. An id prefix pins the id you read only until another changeset shares it; then it is E\_AMBIGUOUS\_ID.

After E\_CAS\_MISMATCH, land your change instead (swarmops land merges it onto the ref's head), or re-apply your edits to the content at the error's `actual`, commit with --parent &lt;actual&gt;, then retry with --expect &lt;actual&gt;. Do not re-send your old whole-file puts: they would revert the winner's edits to shared files.

Example: [Operating SwarmOps: One writer per trunk](../operations.md#one-writer-per-trunk).

## swarmops ref delete

Delete ref NAME.

Envelope `op`: `ref_delete`.

```text
swarmops ref delete [OPTIONS] --expect <REV> <NAME>
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `NAME` | yes | The ref. |  |

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `--expect <REV>` | yes | The changeset the ref holds now: a full id, not a ref name. |  |

`--expect <REV>`:

The changeset the ref holds now.

Give the full changeset id you read (from ref list, show, log or a commit's result). A ref name or a relative revision such as main\~1 is read from the store when the command runs instead of pinning the id you read: the ref's own name always matches (a deleted ref errors instead), so the compare-and-swap protects nothing, and main\~1 on main never matches. An id prefix pins the id you read only until another changeset shares it; then it is E\_AMBIGUOUS\_ID.

After E\_CAS\_MISMATCH, the ref holds the error's `actual`, which someone else moved it to: look at it (swarmops show &lt;actual&gt;) before you delete it with --expect &lt;actual&gt;.

Example: [Operating SwarmOps: One writer per trunk](../operations.md#one-writer-per-trunk).

## swarmops op log

List the store's operations, newest first: each write transaction, its changesets and its ref moves.

Envelope `op`: `op_log`.

```text
swarmops op log [OPTIONS]
```

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `--before <OP>` | no | List the operations numbered below OP: the last op of a page reads on from there. |  |
| `-n, --limit <N>` | no | The most operations to list. | `50` |
| `--max-changesets <N>` | no | The most changesets to list for each operation. | `20` |

Example: [Operating SwarmOps: Look at what landed](../operations.md#3-look-at-what-landed).

## swarmops op undo

Move every ref operation OP moved back to what it named before, all or none.

Each ref must still name what the operation set it to (or what it named before, which is skipped): one that was moved since stops the undo with E\_CAS\_MISMATCH, and nothing is written. An undo run again is a replay (already\_applied). Changesets are never removed.

Envelope `op`: `undo`.

```text
swarmops op undo [OPTIONS] <OP>
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `OP` | yes | The operation, as op log numbers it. |  |

## swarmops verify

Check the store's consistency.

Envelope `op`: `verify`.

```text
swarmops verify [OPTIONS]
```

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `--deep` | no | Re-hash every object, and check the derived values against the objects, too. |  |

Example: [Operating SwarmOps: Checking a store](../operations.md#checking-a-store).

## swarmops git import

Bring a git repository's history in: every commit --ref reaches that the store does not hold becomes a changeset, parents first, and --into moves to the changeset of --ref's commit, created if it does not exist.

The repository is read on this host, at PATH, relative to the working directory: a work tree, which holds .git, or a bare repository. Each changeset keeps its git commit's id in its git.oid metadata, and its author and committer in its signatures; it records no agent. Run again, it takes only the commits after the one --into's head holds, and writes nothing when that is --ref's commit. A head that holds no git commit, or one --ref does not reach, fails with E\_GIT\_IMPORT\_DIVERGED (exit 3), and nothing is written.

Content a changeset cannot hold as git has it is refused by class (E\_GIT\_IMPORT\_LOSSY, naming the commit, the path and the class) unless --lossy allows that class: then it is dropped or normalized, and the changeset names the class in its git.lossy metadata.

Envelope `op`: `git_import`.

```text
swarmops git import [OPTIONS] <PATH>
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `PATH` | yes | The git repository's directory. |  |

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `--ref <GIT_REF>` | no | The commit to import, with every commit it reaches: HEAD, a ref's name or a commit id. | `HEAD` |
| `--into <REF>` | no | The ref to move to that commit's changeset: main or release/&lt;name&gt;. | `main` |
| `--lossy <CLASS>...` | no | A class of content to drop or normalize instead of refusing it, or all; repeatable. Values: `gitlinks`: A submodule's entry: dropped; `paths`: A name that is not UTF-8, or is . or ..: dropped; `modes`: A mode git does not write, such as 100664: normalized; `tree_order`: A tree out of git's order, or naming an entry twice: put in order; `empty_trees`: An entry naming an empty tree: dropped; `nonstandard`: A commit whose bytes the changeset cannot keep: kept as its fields hold it; `all`: Every class. |  |
| `--gitlinks <WHAT>` | no | What to do with a submodule's entry: refuse it, or drop it as --lossy gitlinks does; given, it decides that class whatever --lossy says. Values: `refuse`: Refuse the commit that holds one (E\_GIT\_IMPORT\_LOSSY); `drop`: Drop it, and name the class in the changeset's git.lossy metadata. |  |

Example: [Operating SwarmOps: Once: bring the repository in](../operations.md#once-bring-the-repository-in).

## swarmops git export

Send the store's history to git: every commit of --ref's history that the last export into --branch does not reach is written into the git repository at PATH, parents first, made durable, and --branch moves to the export of --ref's head, created if it does not exist.

The repository is written on this host, at PATH, relative to the working directory: a bare repository (git init --bare makes one), or a work tree, which holds .git and whose checked-out branch is refused (E\_INVALID\_ARG). A changeset imported from git exports to the commit it came from, byte for byte; one made in the store exports to a commit with its own signatures and SwarmOps-\* trailers, the same commit every time. Run again, it writes only what the branch does not reach, and nothing when the branch is at the export of --ref's head. It needs no identity.

A branch at a commit that is no export of --ref's head or of an ancestor of it fails with E\_GIT\_EXPORT\_DIVERGED (exit 3), details.reason saying why, and nothing is written; --force moves it all the same, still by compare-and-swap. Then git push sends the branch on.

Envelope `op`: `git_export`.

```text
swarmops git export [OPTIONS] <PATH>
```

Arguments:

| Argument | Required | Description | Default |
| --- | --- | --- | --- |
| `PATH` | yes | The git repository's directory. |  |

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `--ref <REF>` | no | The store ref whose history to export: main or release/&lt;name&gt;. | `main` |
| `--branch <BRANCH>` | no | The git branch to write, refs/heads/&lt;BRANCH&gt;: main, or a trunk named otherwise, such as stable. | `main` |
| `--force` | no | Move a branch that diverged all the same, replacing what git holds there (by compare-and-swap; never a checked-out branch). |  |

Example: [Operating SwarmOps: Send it to git for review](../operations.md#4-send-it-to-git-for-review).

## swarmops mcp

Serve the store's operations to an agent as an MCP server on stdin and stdout.

One tool per operation (repo\_info, resolve\_rev, read\_file, read\_tree, show, log, path\_history, diff, op\_log, ref\_list, ref\_set, ref\_delete, undo, commit, land, blob\_put, blob\_has, verify), each taking the operation's request as its arguments and returning the envelope the other commands print. The store and the identity are the server's, --repo and --as or their variables, never a call's. Speaks MCP 2026-07-28, and 2025-11-25 and 2025-06-18 to clients that open with initialize. Prints no envelope: stdout carries the protocol until stdin ends, and a command line it refuses has its envelope on stderr. The derive command has no tool: it is maintenance, no agent's work.

Envelope `op`: `mcp`.

```text
swarmops mcp [OPTIONS]
```

Example: [Operating SwarmOps: Give agents the store](../operations.md#give-agents-the-store).

## swarmops derive

Derive the eager derived values of new changesets, ahead of any reader.

Once, it derives the values of the changesets the journal records in up to 1,000 frames, from its first frame or after --after N, and gives the last frame it read as next: run it with --after next to go on. With --follow it runs until it is killed: from the journal's end (from its first frame with --from-start, or after --after N) it derives the changesets as they are recorded, looking again every --interval-ms once it has caught up, and prints one envelope a line for each run that derived something. A run that fails ends it with that run's envelope and exit code. Commits derive nothing.

Envelope `op`: `derive`.

```text
swarmops derive [OPTIONS]
```

Options:

| Option | Required | Description | Default |
| --- | --- | --- | --- |
| `--after <N>` | no | Go on after journal frame N, a previous run's next (default: the first frame; with --follow, the journal's end). |  |
| `--follow` | no | Keep deriving what the journal records, until killed. |  |
| `--interval-ms <N>` | no | With --follow: how many milliseconds to wait before looking again, once caught up. | `500` |
| `--from-start` | no | With --follow: start from the journal's first frame, not its end. |  |

Example: [Operating SwarmOps: Making room](../operations.md#making-room).
