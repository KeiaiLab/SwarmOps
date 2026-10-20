# Operating SwarmOps

This guide is for whoever runs SwarmOps for a swarm of coding agents on one host. It installs the
binary, creates a store, gives agents the store through MCP, runs the daily loop between the store
and the git repository that CI and deployment watch, checks and backs up the store, and says what
an upgrade means, where the limits are and what to watch. [pilot.md](pilot.md) runs the same loop
once on this repository, as the first real use.

Every command below was run with the binary built from this repository, and every output is
copied from those runs, cut down where it is long: `…` marks a cut, and the store's path in
`meta.repo`, which echoes the path the caller gave, is left out. `swarmops <command> --help` is
the authority on flags, and the README's [Commands](../README.md#commands) table lists them all.
The examples run the long name, `swarmops`; `so` is the same program.

## Before you start

- **Linux.** SwarmOps is built and tested on x86_64 Linux
  ([platforms](policies/msrv-and-platforms.md)).
- **One host.** Every agent, every MCP server and every command that uses a store runs on the
  host that holds it: SwarmOps has no network protocol yet (remote sync is M11). Between hosts,
  git carries the history, one writer at a time ([One writer per trunk](#one-writer-per-trunk)).
- **A native Linux filesystem for the store**: ext4 with its journal (what SwarmOps is tested and
  measured on), XFS or btrfs. A store under `/mnt/` (where WSL mounts Windows drives) is refused
  with `E_UNSUPPORTED_FS`; NFS, SMB and FUSE mounts are not detected yet, and not supported.
- **One Unix user.** The store's object files, ref files and checkpoint are created readable by
  their owner only (mode `0600`), so every agent, MCP server and command that uses a store runs as
  the user who owns it. The store directory's filesystem permissions are its only access control
  ([SECURITY.md](../SECURITY.md)).
- **git** on the host, for `clone`, `fetch` and `push`. `swarmops git import` and
  `swarmops git export` read and write git repositories themselves.

## Install

### From a clone

The repository's `rust-toolchain.toml` pins Rust 1.99.0, which rustup installs on the first build
(rustup may warn that the toolchain file overrides your default toolchain: that is the pin at
work). The zstd and BLAKE3 crates compile C, so the host also needs a C compiler.

```sh
gh repo clone KeiaiLab/SwarmOps
cd SwarmOps
cargo install --locked --path crates/swarmops-cli
swarmops --version
```

```text
  Installing …/bin/so
  Installing …/bin/swarmops
   Installed package `swarmops-cli v0.1.0 (…/SwarmOps/crates/swarmops-cli)` (executables `so`, `swarmops`)
swarmops 0.1.0
```

The repository is private for now, so `gh` clones it with your GitHub login. `cargo install`
builds in release mode, without the `test-hooks` feature, and installs both names into
`~/.cargo/bin` (add `--bin swarmops` for the long name alone); it took 20 to 36 s of wall time on
a 32-thread host. To install another commit, check it out and run the same `cargo install`: it
replaces both binaries (`Replaced package`).

### From a release

From v0.1.0 on, each release on the
[releases page](https://github.com/KeiaiLab/SwarmOps/releases) carries one archive per Linux
target, `swarmops-<version>-x86_64-unknown-linux-gnu.tar.gz` and
`swarmops-<version>-aarch64-unknown-linux-gnu.tar.gz` (`swarmops`, `so`, the license, the README
and the changelog), and a `SHA256SUMS` file to check them against. The binaries need glibc 2.34
or newer. [RELEASE.md](../RELEASE.md) has the release steps.

```sh
gh release download v0.1.0 --repo KeiaiLab/SwarmOps \
  --pattern 'swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz' --pattern SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz
install -D -t ~/.local/bin swarmops-0.1.0-x86_64-unknown-linux-gnu/swarmops \
  swarmops-0.1.0-x86_64-unknown-linux-gnu/so
~/.local/bin/swarmops --version
```

```text
swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz: OK
swarmops 0.1.0
```

Until v0.1.0 is published, `gh release download` fails with `release not found`; the commands
after it were run on an archive packed the way the release workflow packs one.

## Create a store

A store is one directory. Put it on the host's local disk with room to grow: it keeps one file per
object (zstd-compressed when that is smaller), packs nothing and removes nothing yet. A new store
takes about 1 MB (its 256 object directories); SwarmOps' own history, 422 commits, takes 51 MB in
8,085 files. Every command finds the store by `--repo` or `SWARMOPS_REPO`; there is no default
location, and `~/.local/state/swarmops/repos/<name>` is only the convention used here.

```sh
export SWARMOPS_REPO=~/.local/state/swarmops/repos/acme
swarmops init "$SWARMOPS_REPO" --name acme
```

```json
{"ok":true,"op":"repo_init","api":1,"result":{"repo_id":"101d13d0eebd438776f99316f817dad6","name":"acme","store_version":1,"changesets":0,"refs":0},"meta":{"repo":"…","elapsed_ms":52,"resolved":{},"auth":"trust"}}
```

The directory holds `FORMAT` (`swarmops-store 1`, the layout's version), `REPO` (the repository's
id and name), `objects/` and `meta/`; [the README](../README.md#the-store-on-disk) and
[architecture.md](architecture.md) describe them. Nothing runs in the background: there is no
daemon, and each command or MCP call opens the store, does its work and closes it.

## Identities

Reads need no identity. A commit, a land, a ref move, an undo and a git import need one:
`--as <family>/<instance>`, else `SWARMOPS_AGENT` (a family of `[a-z0-9-]`, an instance of
`[A-Za-z0-9._-]` that does not start with `.`), with `SWARMOPS_RUN_ID` naming the agent's run if
it has one. The store records it on the changeset and on each ref move, and `git export` writes it
as the commit's author or committer, `<family>/<instance> <instance@family.swarmops.local>`. Give
every agent process its own instance, and yourself one of your own, such as `operator/ada`.
`git export` needs none.

```sh
swarmops --as operator/ada whoami
```

```json
{"ok":true,"op":"whoami","api":1,"result":{"agent":"operator/ada","auth":"trust"},"meta":{"elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

## Give agents the store

`swarmops mcp` serves the store's operations to an agent as an MCP server on stdin and stdout.
Register it with Claude Code for one agent, with its identity:

```sh
claude mcp add swarmops -e SWARMOPS_AGENT=agent/worker-1 -- swarmops mcp --repo ~/.local/state/swarmops/repos/acme
claude mcp list
```

```text
Added stdio MCP server swarmops with command: swarmops mcp --repo … to local config
…
swarmops: swarmops mcp --repo … - ✔ Connected
```

That registration is Claude Code's `local` scope: private to you, in the directory you ran it in.
For a project that several agent sessions open, one `.mcp.json` at its root registers the server
for every one of them, and each session gives its own identity in its own `SWARMOPS_AGENT`, which
Claude Code expands into the server's environment:

```json
{
  "mcpServers": {
    "swarmops": {
      "command": "swarmops",
      "args": ["mcp", "--repo", "/home/me/.local/state/swarmops/repos/acme"],
      "env": {"SWARMOPS_AGENT": "${SWARMOPS_AGENT:-}"}
    }
  }
}
```

Claude Code asks once, in each project, to approve the servers a `.mcp.json` names; until then
`claude mcp list` shows the server as `⏸ Pending approval`. The README's [MCP](../README.md#mcp)
section says why the entry runs `swarmops` rather than `so`, and why the empty default (`:-`)
matters. Give the store's path absolute, as above.

### The tools agents get

The server lists 18 tools, one per operation, each taking the operation's request as its
arguments and returning the envelope the CLI prints for it:

| Reads | Writes |
| --- | --- |
| `repo_info`, `resolve_rev`, `read_file`, `read_tree`, `show`, `log`, `path_history`, `diff`, `op_log`, `ref_list`, `blob_has`, `verify` | `commit`, `land`, `blob_put`, `ref_set`, `ref_delete`, `undo` |

`init`, `whoami`, `schema`, `git import`, `git export` and `derive` are commands only: you run
them, not the agents.

- The store and the identity are the server's: `--repo` (else `SWARMOPS_REPO`) and `--as` (else
  `SWARMOPS_AGENT`). No call can name others, and without an identity every write fails with
  `E_IDENTITY_REQUIRED`.
- Claude Code starts one server for a session, and the session's subagents share it and its
  identity. An agent that needs an identity of its own runs as its own Claude Code process.
- The server opens the store for each call, so it sees what other agents commit meanwhile. It
  exits on any of the store's four documented aborts (storage spec §2; the README's
  [MCP](../README.md#store-identity-and-lifetime) section lists them), such as a failed journal
  write whose cut-back fails too: the client sees the server exit, and has to start it again.
- The server tells the model how to use it, in its MCP `instructions` and its tools'
  descriptions: a full 64-hex changeset id wherever a compare-and-swap needs one, `land` to
  publish (on `E_CONFLICT`, `details.report` says what to resolve), an `idempotency_key` to make a
  retry safe, and small pages (`max_bytes` of 32768 or less, a `limit` of 100 or less), since
  clients limit long tool results. Put the same in the project's `CLAUDE.md` if you want it in
  every prompt.
- Agents work without a checkout: they read files through `read_file` and `read_tree`, and write
  whole files through `commit`. There are no workspaces yet (M5), so an agent that must build or
  test its change needs the files on disk some other way, such as a git checkout of an export.

## The daily loop

The loop keeps two copies of the history in step: the store, where agents commit and land, and
the git repository that people, CI and deployment use. The store's side of git is a bare clone
of that repository, the *git side*: imports read it, exports write it, and `git fetch` and
`git push` move commits between it and the remote. The examples call the repository `acme`, its
trunk `stable`, and its remote `git@github.com:acme/acme.git` (a stand-in: the runs behind this
guide used a local bare repository as the remote).

### Once: bring the repository in

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
`--ref` and `--into` name the git side's branch and the store's ref (`--ref` defaults to `HEAD`,
`--into` to `main`). Content a changeset cannot hold as git has it (a submodule, a name that is
not UTF-8, a mode or a tree order git does not write, an empty directory, a commit whose bytes
its fields cannot give back) is refused with `E_GIT_IMPORT_LOSSY`, naming the commit, the path
and the class, until you allow that class with `--lossy`
([README](../README.md#bring-a-git-repository-in)). SHA-256 repositories, reftable refs and
shallow clones are refused.

### 1. Take in what git gained

```sh
git -C "$GIT_SIDE" fetch origin stable:stable
swarmops git import "$GIT_SIDE" --ref stable --into main
```

```json
{"ok":true,"op":"git_import","api":1,"result":{"imported":0,"skipped":1,"head_changeset":"c893148f…","git_head_sha":"84226f3190a555751039017a08ef8485fd9f514b","target_ref":"main","ref_moved":false,"old_head":"c893148f…","lossy":[],"lossy_total":0},"meta":{"repo":"…","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

With nothing new, the import writes nothing. Commits pushed to the trunk on top of the last export
come in, and `main` moves to the last of them (`"imported":1` for one). The fetch moves the git
side's `stable` only forward. When it refuses (`! [rejected] stable -> stable
(non-fast-forward)`, exit 1), the git side holds commits the remote lacks: an export whose push
did not happen (push it, then fetch again), or a trunk that moved on both sides
([One writer per trunk](#one-writer-per-trunk)).

### 2. Agents commit and land

Agents do this through the `commit` and `land` tools. The same requests at the command line, as an
operator or a script makes them, read `main`'s head, commit a change on it, and land it. `field`
prints one field of an envelope:

```sh
field() { python3 -I -c 'import functools, json, sys; print(functools.reduce(lambda v, k: v[k], sys.argv[1].split("."), json.load(sys.stdin)))' "$1"; }
BASE=$(swarmops resolve main | field result.id)
cat change.json
swarmops --as agent/worker-1 commit -m "greet the swarm" --changes change.json --parent "$BASE"
swarmops --as agent/worker-1 land c3828a37d7969df68d60dd152e953e0ffd249dd4b56862c8e9b81051b932d6fe --key "$(uuidgen)"
```

```text
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn greet() -> &'static str {\n    \"hello, swarm\"\n}\n"}}]
{"ok":true,"op":"commit","api":1,"result":{"changeset":"c3828a37d7969df68d60dd152e953e0ffd249dd4b56862c8e9b81051b932d6fe","parent":"c893148f…","root":"ff7ba3ee…","files_changed":1,"blobs_written":1,"replayed":false},"meta":{"repo":"…","elapsed_ms":23,…}}
{"ok":true,"op":"land","api":1,"result":{"status":"landed","landed":"9a3a41fc3248cffdd4240292ec6468bf117817dd5df0c57268ba811e8fd947c1","rebased_onto":"c893148f…","target":"main","base":"c893148f…","attempts":1,"relocated":false,"files_changed":1,"squashed_from":["c3828a37…"]},"meta":{"repo":"…","elapsed_ms":29,…}}
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
    "message": "1 path(s) of the change conflict with the target's head 9a3a41fc3248cffdd4240292ec6468bf117817dd5df0c57268ba811e8fd947c1: nothing was recorded",
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
            "theirs_changeset": "9a3a41fc3248cffdd4240292ec6468bf117817dd5df0c57268ba811e8fd947c1"
          }
        ],
        "kinds": [
          "content"
        ],
        "ours": "b5c51808ba15fa85151e4dc355e03002cfda8ef2f166c0b0f193cd4365a7c77b",
        "theirs": "9a3a41fc3248cffdd4240292ec6468bf117817dd5df0c57268ba811e8fd947c1",
        "truncated": false
      }
    },
    "hint": "swarmops cat 9a3a41fc…:src/lib.rs, then swarmops land b5c51808… --resolve <file> with each path's resolution and against = 9a3a41fc…"
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
{"against": "9a3a41fc3248cffdd4240292ec6468bf117817dd5df0c57268ba811e8fd947c1",
 "files": {"src/lib.rs": {"content": {"inline": "pub fn greet() -> &'static str {\n    \"hi, swarm\"\n}\n"}}}}
{"ok":true,"op":"land","api":1,"result":{"status":"landed","landed":"3f7e703de718820bb6e7a620e201b6c0e5215ec097fc443f183b7184bc734689","rebased_onto":"9a3a41fc…","target":"main","base":"c893148f…","attempts":1,"relocated":false,"files_changed":2,"squashed_from":["b5c51808…"]},"meta":{"repo":"…","elapsed_ms":31,…}}
```

Through MCP, the `land` tool takes the same resolutions as its `resolutions` argument.

### 3. Look at what landed

```sh
swarmops log -n 3 --human
swarmops diff "$BASE" main --format stat --human
swarmops op log -n 2 --human
```

```text
entries[0].id: 3f7e703de718820bb6e7a620e201b6c0e5215ec097fc443f183b7184bc734689
entries[0].parents[0]: 9a3a41fc3248cffdd4240292ec6468bf117817dd5df0c57268ba811e8fd947c1
entries[0].author_agent: agent/worker-2
entries[0].created_at: 1792471124000
entries[0].title: a shorter greeting
…
from: c893148f550e68a1d0c0dfcf9bd10b4a738b8f60245ddcc94f4dbf0cb5d67755
to: 3f7e703de718820bb6e7a620e201b6c0e5215ec097fc443f183b7184bc734689
files[0].path: README.md
files[0].status: modified
files[0].binary: false
files[0].insertions: 1
files[0].deletions: 1
files[1].path: src/lib.rs
…
ops[0].op: 5
ops[0].kind: land
ops[0].changesets[0].id: 3f7e703de718820bb6e7a620e201b6c0e5215ec097fc443f183b7184bc734689
ops[0].changesets[0].agent: agent/worker-2
…
ops[0].ref_moves[0].name: main
ops[0].ref_moves[0].old: 9a3a41fc3248cffdd4240292ec6468bf117817dd5df0c57268ba811e8fd947c1
ops[0].ref_moves[0].new: 3f7e703de718820bb6e7a620e201b6c0e5215ec097fc443f183b7184bc734689
ops[0].ref_moves[0].actor: agent/worker-2
…
```

`op log` numbers every write transaction; `swarmops op undo <op>` moves the refs one of them moved
back, by compare-and-swap, all or none ([README](../README.md#first-commands)).

### 4. Send it back to git

```sh
swarmops git export "$GIT_SIDE" --ref main --branch stable
git -C "$GIT_SIDE" push origin stable
```

```text
{"ok":true,"op":"git_export","api":1,"result":{"exported":2,"objects_written":9,"head_changeset":"3f7e703d…","head_sha":"514ab2b6e63ccfd51947b38d741ff60557e0996f","branch":"stable","old_sha":"84226f3190a555751039017a08ef8485fd9f514b","branch_moved":true,"incremental":true,"rewritten":0},"meta":{"repo":"…","elapsed_ms":29,…}}
To …
   84226f3..514ab2b  stable -> stable
```

The export writes every changeset the branch does not reach as a git commit, makes the objects
durable, then moves the branch by compare-and-swap; run again, it writes nothing. The push is a
fast-forward, and the remote's CI and deployment run on it as they run on any push. What they see:

```sh
git -C "$GIT_SIDE" log --format="%h %an <%ae>%n    %s" -3 stable
git -C "$GIT_SIDE" log -1 --format=%B stable
```

```text
514ab2b agent/worker-2 <worker-2@claude-code.swarmops.local>
    a shorter greeting
1a76c9e agent/worker-1 <worker-1@claude-code.swarmops.local>
    greet the swarm
84226f3 Ada Lovelace <ada@example.com>
    docs: say what acme does
a shorter greeting

SwarmOps-Changeset: 3f7e703de718820bb6e7a620e201b6c0e5215ec097fc443f183b7184bc734689
SwarmOps-Land-Key: 370d9a8b-aa45-4ab7-b458-2bcc992ff0b9
SwarmOps-Squashed-From: b5c51808ba15fa85151e4dc355e03002cfda8ef2f166c0b0f193cd4365a7c77b
```

Commits imported from git come back as themselves, byte for byte. A changeset made in the store
becomes a commit whose author and committer are the identities the store recorded for it (an
agent's, with a `swarmops.local` address), with SwarmOps' trailers after the message and no
signature: a pipeline or branch rule that requires signed commits or known authors refuses it. A
message that already ends in a trailer block, such as `Co-Authored-By:`, keeps it, and the
SwarmOps trailers join that block. Tags stay git's: tag an exported commit in git, as before.

### One writer per trunk

Between an import and the next export, only one side may change the trunk. People change it in
one of three ways that keep to this:

- through SwarmOps, with an identity of their own, as an agent does;
- on the trunk in git, after an export and before the next import, which step 1 then takes in;
- on a branch, which you import into a ref of its own and land:

```sh
git -C "$GIT_SIDE" fetch origin feature/farewell:feature/farewell
swarmops git import "$GIT_SIDE" --ref feature/farewell --into release/farewell
swarmops land release/farewell --key "$(uuidgen)"
swarmops ref delete release/farewell --expect "$(swarmops resolve release/farewell | field result.id)"
```

The land squashes the branch's commits into one changeset on `main`, whose author is the author of
the branch's last commit and whose committer is you; the next export and push take it to the trunk
as a fast-forward:

```sh
swarmops git export "$GIT_SIDE" --ref main --branch stable
git -C "$GIT_SIDE" push origin stable
git -C "$GIT_SIDE" log -1 --format="%an <%ae> / %cn <%ce>%n%B" stable
```

```text
{"ok":true,"op":"git_export","api":1,"result":{"exported":1,"objects_written":1,"head_changeset":"212f45be…","head_sha":"5c7ca91acf8488b741ad5454e583eed962d684c1","branch":"stable","old_sha":"89013bf44846356cf0b8f7ac67dacf6f1073f6b5","branch_moved":true,"incremental":true,"rewritten":0},…}
To …
   89013bf..5c7ca91  stable -> stable
Ada Lovelace <ada@example.com> / operator/ada <ada@operator.swarmops.local>
farewell: say goodbye

SwarmOps-Changeset: 212f45beae9f9bd0ae37ea7a46b47e533e0ae38f5a62caad42e518bf58163d0b
SwarmOps-Land-Key: f247f8f7-e6d2-4151-b22b-90fb6bddcb17
SwarmOps-Squashed-From: 6ec5686168ec918f06a8094ad0affedd9a447221e8e2e8c5fa9aee461ff01878
```

When both sides moved anyway (here an agent landed a change while a person pushed one to the
trunk), nothing is merged: step 1's import fails with `E_GIT_IMPORT_DIVERGED`, and the export
with `E_GIT_EXPORT_DIVERGED`, reason `both_moved` (exit 3 for both), and neither writes anything:

```sh
swarmops git export "$GIT_SIDE" --ref main --branch stable --pretty
```

```text
{
  "ok": false,
  "op": "git_export",
  "api": 1,
  "error": {
    "code": "E_GIT_EXPORT_DIVERGED",
    "message": "the branch stable has diverged from 60edce26a65510fe87d030a604b49475c949f50ebe84925f104f9bb3a47f7f56 (both_moved): it names 1b3a9f40cbcdb63ac3eaee57d962c93ecd35286f",
    "retryable": false,
    "exit": 3,
    "details": {
      "branch": "stable",
      "exported_changeset": "212f45beae9f9bd0ae37ea7a46b47e533e0ae38f5a62caad42e518bf58163d0b",
      "git_head": "1b3a9f40cbcdb63ac3eaee57d962c93ecd35286f",
      "head_changeset": "60edce26a65510fe87d030a604b49475c949f50ebe84925f104f9bb3a47f7f56",
      "reason": "both_moved"
    },
    "hint": "both git and main moved past 212f45beae9f9bd0ae37ea7a46b47e533e0ae38f5a62caad42e518bf58163d0b, and nothing is merged: git -C … log refs/heads/stable shows the git side, and --force replaces it"
  },
  …
}
```

`details.exported_changeset` is the last changeset both sides share. To keep git's history as it
is, with no forced push, put the store's new work on top of git's: import the trunk into a ref of
its own, land `main`'s changes since the shared changeset onto it, move `main` there, export and
push:

```sh
swarmops git import "$GIT_SIDE" --ref stable --into release/git
HEAD_WAS=$(swarmops resolve main | field result.id)
LANDED=$(swarmops land main --base 212f45beae9f9bd0ae37ea7a46b47e533e0ae38f5a62caad42e518bf58163d0b --target release/git --key "$(uuidgen)" | field result.landed)
swarmops ref set main "$LANDED" --expect "$HEAD_WAS"
swarmops git export "$GIT_SIDE" --ref main --branch stable
git -C "$GIT_SIDE" push origin stable
swarmops ref delete release/git --expect "$LANDED"
```

That land squashes all of `main`'s changes since the shared changeset into one changeset. The other
way is `git export --force`, which replaces git's side with the store's: the push that follows is
a forced one, and the commits made in git are gone from the trunk.

The export's other reasons, each with its hint: `branch_ahead` (commits made in git on top of the
export: step 1 takes them in), `branch_moved` (the branch names no export of this history),
`source_moved_back` (`main` moved back, as `op log` shows), `lossy_history` (the branch holds the
commits a lossy import was made of: export to another branch) and `moved_during_export` (run it
again). An export that finds `refs/heads/<branch>.lock` in the git side, which a git process holds
or a dead one left, fails with `E_GIT_WRITE` (`ref_locked`) until the file is removed, once no git
process runs.

## Checking a store

```sh
swarmops verify
swarmops verify --deep
```

```json
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":42,"changesets_checked":14,"problems":[],"findings":[]},"meta":{"repo":"…","elapsed_ms":2,"resolved":{},"auth":"trust"}}
```

`verify` checks the structure: the journal, the index, the refs, every changeset and tree, the
external id entries, and that every blob is there. `--deep` also re-hashes every object file. On
SwarmOps' own history (7,647 objects) they took 0.1 s and 0.3 s. A store with damage fails with
`E_STORE_CORRUPT` (exit 1), each damaged object, record or ref in `details.findings`; this is
`verify --deep --pretty` on a copy of the store with one byte of one object file changed:

```text
{
  "ok": false,
  "op": "verify",
  "api": 1,
  "error": {
    "code": "E_STORE_CORRUPT",
    "message": "verify found a problem: object 0112121a9368ae739fcf8af5e4a1dd4f94d76a6afa38135b1ba08cc5cdadbbfa is corrupt",
    "retryable": false,
    "exit": 1,
    "details": {
      "changesets_checked": 14,
      "findings": [
        {
          "kind": "corrupt_object",
          "object": "0112121a9368ae739fcf8af5e4a1dd4f94d76a6afa38135b1ba08cc5cdadbbfa"
        }
      ],
      "objects_checked": 42,
      "problems": [
        "object 0112121a9368ae739fcf8af5e4a1dd4f94d76a6afa38135b1ba08cc5cdadbbfa is corrupt"
      ]
    },
    "hint": "details.findings names each damaged object, record or ref by kind and id: restore them from a backup of the store"
  },
  …
}
```

There is no repair tool yet (M6): stop the writers, keep the damaged store aside, and restore
(below). Run `verify --deep` after a crash or a power loss, on every backup, before an upgrade,
and on a schedule: it reads the whole store, and holds the journal and the index in memory.

## Backup and restore

What a copy may rely on comes from the store's own guarantees (storage-core spec §2 and §3):

- **Objects never change.** An object is written to `objects/tmp/`, synced, then linked into
  `objects/<2 hex>/<62 hex>`; no one rewrites or removes it afterwards.
- **The journal is the truth.** `meta/journal.log` records every write transaction as one frame,
  and a write is committed once its frame is synced. The index, the ref files and the checkpoint
  are projections of the journal, which the next writer brings up to date.
- **Every metadata write holds `meta/lock`**, an `flock`. Object writes take no lock, but no frame
  names an object before the object is durable. Readers take no lock.
- **A crash at any point leaves a store that the next writer repairs.**

So:

- **A copy made while you hold `meta/lock` is consistent.** `flock` takes the lock the writers
  take: while it runs, no frame is appended and no ref moves, and the objects that writers add
  meanwhile are ones no frame in the copy names. Writers wait up to 10 s for the lock, then fail
  with the retryable `E_BUSY`: hold it briefly, or pause the agents.
- **An atomic snapshot of the store's filesystem** (an LVM snapshot of its volume, or a btrfs
  snapshot of its subvolume) holds what a crash of every writer at that instant would leave, which
  the next writer repairs.
- **A plain copy of a store in use is not safe.** `cp`, `rsync` or `tar` without the lock can copy
  a ref file that names a changeset newer than the journal it copied, a state no crash produces.

A backup, under the lock, and its check (`BACKUP` must not exist yet: `cp -a` would copy the store
into it):

```sh
BACKUP=~/backups/acme-$(date +%F)
mkdir -p ~/backups
flock "$SWARMOPS_REPO/meta/lock" cp -a "$SWARMOPS_REPO" "$BACKUP"
swarmops --repo "$BACKUP" verify --deep
```

```json
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":42,"changesets_checked":14,"problems":[],"findings":[]},"meta":{"repo":"…","elapsed_ms":2,"resolved":{},"auth":"trust"}}
```

For a large store, copy the objects first without the lock (they never change), then hold the
lock only while `rsync` copies the rest and the objects added since; `objects/tmp/` holds
unfinished writes and is left out (a store creates it again when it writes):

```sh
mkdir -p "$BACKUP"
rsync -a --exclude=/tmp/ "$SWARMOPS_REPO/objects/" "$BACKUP/objects/"
flock "$SWARMOPS_REPO/meta/lock" rsync -a --delete --exclude=/objects/tmp/ "$SWARMOPS_REPO/" "$BACKUP/"
swarmops --repo "$BACKUP" verify --deep
```

A writer that meets the held lock waits, then gives up:

```json
{"ok":false,"op":"commit","api":1,"error":{"code":"E_BUSY","message":"the metadata store is busy: another writer holds meta/lock","retryable":true,"exit":4,"details":{},"hint":"retry: another writer held the store's lock for the whole wait"},"meta":{"repo":"…","elapsed_ms":10017,…}}
```

To restore, stop every writer first (the agents' sessions and their MCP servers, and any script),
put the damaged store aside, copy the backup into place and check it. Then take back from git
what was exported after the backup:

```sh
mv "$SWARMOPS_REPO" "$SWARMOPS_REPO.damaged"
cp -a "$BACKUP" "$SWARMOPS_REPO"
swarmops verify --deep
git -C "$GIT_SIDE" fetch origin stable:stable
swarmops git import "$GIT_SIDE" --ref stable --into main
swarmops git export "$GIT_SIDE" --ref main --branch stable
```

```text
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":42,"changesets_checked":14,"problems":[],"findings":[]},…}
{"ok":true,"op":"git_import","api":1,"result":{"imported":1,"skipped":1,"head_changeset":"4a7add92…","git_head_sha":"fd35e482e2639549ce8558c0e811e909fa26eb7d","target_ref":"main","ref_moved":true,"old_head":"4b617abe…","lossy":[],"lossy_total":0},…}
{"ok":true,"op":"git_export","api":1,"result":{"exported":0,"objects_written":0,…,"branch_moved":false,"incremental":true,"rewritten":0},…}
```

The land exported after the backup comes back as the commit it was exported as, and the export
finds nothing to write. It is an imported changeset now: `log` shows its agent as
`unknown/unknown`, and its signatures keep the git author, the agent that made it. What is lost is
what was written after the backup and never exported: changesets no export reached (an agent's
unlanded commits, lands made since the last export), ref moves, and the operation log's entries.

The git side is itself a copy of everything exported. Without any backup, a new store made from it
(`swarmops init`, then `swarmops git import` of each branch) exports to the same commits.

## Upgrades

Every store today is `FORMAT 1`:

```sh
cat "$SWARMOPS_REPO/FORMAT"
```

```text
swarmops-store 1
```

A binary opens only a store whose `FORMAT` it knows, and refuses any other as `E_NOT_A_REPO`
(exit 6). This is a store whose `FORMAT` file was edited to a layout this build does not know:

```json
{"ok":false,"op":"repo_info","api":1,"error":{"code":"E_NOT_A_REPO","message":"…/FORMAT names a store layout this build does not read","retryable":false,"exit":6,"details":{},"hint":null},…}
```

- **A new binary with the same `FORMAT`** is an install over the old one (above). Running MCP
  servers keep the binary they started with until their sessions end. Check the store with
  `verify --deep` after the upgrade.
- **A new `FORMAT`** comes with the release that needs it, and [CHANGELOG.md](../CHANGELOG.md)
  says so. The next one (planned as 2, in M6) adds a header to every journal frame and a checksum
  to every `journal.off` entry. An older binary then refuses the new store rather than misread it,
  because nothing skips a record it does not know. Before 1.0 there is no migration tool
  ([versioning policy](policies/versioning-and-stability.md)): a store may have to be created
  again.
- **Creating a store again** goes through git: export every ref you keep (`main` to the trunk,
  each `release/<name>` to a branch of its own), make the new store with the new binary, and import
  each branch into its ref. The new store exports to the same commits. What does not carry over:
  changesets no ref reaches, the operation log (and so `undo`), and the changeset ids of the
  changesets made in the store, which come back as imported changesets with ids of their own.

```sh
NEW=~/.local/state/swarmops/repos/acme-next
swarmops init "$NEW" --name acme
swarmops --repo "$NEW" git import "$GIT_SIDE" --ref stable --into main
swarmops --repo "$NEW" git export "$GIT_SIDE" --ref main --branch stable
```

```text
{"ok":true,"op":"repo_init","api":1,"result":{"repo_id":"967ad1e0bc69cbfaa9c648643de94032","name":"acme","store_version":1,"changesets":0,"refs":0},…}
{"ok":true,"op":"git_import","api":1,"result":{"imported":10,"skipped":0,"head_changeset":"16c363e3…","git_head_sha":"fd35e482e2639549ce8558c0e811e909fa26eb7d","target_ref":"main","ref_moved":true,"lossy":[],"lossy_total":0},…}
{"ok":true,"op":"git_export","api":1,"result":{"exported":0,"objects_written":0,…,"head_sha":"fd35e482e2639549ce8558c0e811e909fa26eb7d","branch":"stable",…,"branch_moved":false,…,"rewritten":0},…}
```

The hash contract (the byte layouts of blobs, trees and changesets, and the hash recipe) never
changes: a changeset's id is the same under every `FORMAT`.

## Limits

The ones an operator meets first; [known-limitations.md](known-limitations.md) lists them all,
each with the milestone that takes it.

- **One host, one writer per trunk.** No sync between hosts until M11; git carries history between
  them, with one host landing and exporting and the others importing.
- **No workspaces** (M5): agents read and write through the tools, a whole file per change.
- **Nothing is collected** (M6): `objects/tmp/` keeps the leftovers of interrupted writes, and
  `meta/derived/` every derived value. A store only grows.
- **No repair tool** (M6): `verify` reports damage, and a restore fixes it.
- **Writers take turns.** Every write holds one store-wide lock across two journal syncs: about
  21 ms per uncontended commit on the maintainers' host. With 32 writers committing in a loop, a
  commit can wait a few hundred transactions; on a slow enough disk that passes the 10 s wait for
  the lock, and the commit fails with `E_BUSY`.
- **Exports write loose objects**, one file and one fsync per object, until `git gc` packs them:
  exporting SwarmOps' history into an empty repository wrote 7,651 objects in 12 s on a busy
  host. An export that dies while it moves the branch leaves `refs/heads/<branch>.lock`, and an
  export leaves no reflog entry.
- **Content git can hold and a changeset cannot** (submodules, names that are not UTF-8, odd modes
  and tree orders) is refused by the import unless `--lossy` allows it, and a history imported with
  a loss exports as new commits.

## What to monitor

Every command and every tool result is an envelope, so a log of envelopes, one JSON line per run,
is the monitoring data. What to read in them:

- **Errors by code.** Expected now and then, and handled by the agent: `E_CONFLICT` (exit 3),
  `E_CAS_MISMATCH` (re-read, then land). A rising count of `E_BUSY` or `E_CAS_RETRY_EXHAUSTED`
  (exit 4) means more contention than the lock serves: too many writers, a slow disk, or something
  holding the lock (a backup). Act at once on `E_IO`, `E_IO_TRANSIENT` (their `details` name the
  file operation and the store file), `E_STORE_CORRUPT` and `E_INTERNAL`, and on an MCP server
  that exits.
- **`meta.recovered`** on a write: what it repaired of a writer that died mid-write. Agents get
  killed, so it appears now and then; often, it means something keeps killing writers.
- **`warnings`** in a result: what an operation that succeeded still wants you to know, such as a
  commit that is durable but whose follow-up work failed (the next write finishes it), or an export
  that wrote commits anew because their history was imported with a loss.
- **`meta.elapsed_ms`** of commits and lands: the lock's wait is in it.
- **`verify --deep`**: `problems` and `findings` empty, on a schedule and on every backup.
- **Disk**: free space on the store's filesystem, and the size of `objects/`, `objects/tmp/` and
  `meta/derived/`, which only grow.
- **The git side**: each export's `exported`, `branch_moved` and `rewritten` (0 unless the history
  was imported with a loss), any `E_GIT_EXPORT_DIVERGED` (someone wrote the trunk) or `E_GIT_WRITE`,
  and the CI and deployment runs on each push, as before.
- **`op log`**: who moved which ref, for moves no one expected.
