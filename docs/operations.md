# Operating SwarmOps

This guide is for whoever runs SwarmOps for a swarm of coding agents on one host. It installs the
binary, creates a store, gives agents the store through MCP, runs the daily loop between the store
and the git repository that CI and deployment watch, with a person's review before the trunk moves,
checks, backs up and trims the store, and says what an upgrade means, where the limits are and what
to watch. [pilot.md](pilot.md) runs the same loop once on this repository, as the first real use.

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
  ([SECURITY.md](../SECURITY.md)). Those `0600` files are the objects, the refs, the checkpoint,
  the external ids, `FORMAT` and `REPO`. The rest follow the umask: the store's directories, its
  journal (`meta/journal.log` and `meta/journal.off`), `meta/parents.log`, `meta/lock`, the index
  columns, the sorted views and the derived values (`0775` and `0664` under a umask of `0002`:
  whoever can reach the store's directory can then list it and read its journal, and the store's
  group can write to it). So keep a store meant for one user in a directory of mode `0700`:
  `install -d -m 0700 ~/.local/state/swarmops/repos` before the first `swarmops init` under it
  (on a directory that exists, it sets the mode).
- **git** on the host, for `clone`, `fetch` and `push`. `swarmops git import` and
  `swarmops git export` read and write git repositories themselves.

## Install

### With the installer

```sh
curl -fsSL https://get.swarmops.cc/install.sh | bash
```

`scripts/install.sh`, which swarmops.cc serves, downloads the latest release's archive for this
host's target, checks it against the release's `SHA256SUMS`, and puts `swarmops` and `so` in
`~/.local/bin`, or `/usr/local/bin` when run as root; nothing is installed on a mismatch. It
refuses a host the release builds do not run on (not Linux, not x86_64 or aarch64, musl, or
glibc older than 2.34) before it downloads anything. Each binary is written under a temporary
name in the directory and renamed into place, so an interrupted run leaves the old binary, never
half a new one.

| To | Run |
| --- | --- |
| install a given release | `… \| bash -s -- --version 0.1.0` |
| install elsewhere | `… \| bash -s -- --dir /opt/swarmops/bin` (or `SWARMOPS_INSTALL_DIR`) |
| update | the same command again: it says which version it replaced |
| uninstall | `… \| bash -s -- --uninstall`: removes the two binaries, if they are SwarmOps', and no store |

For a fleet, pin `--version` (or `SWARMOPS_VERSION`) so that every host runs the same release,
and keep a reviewed copy of the script with your provisioning rather than piping it from the
network each time. It needs curl or wget, tar, and sha256sum or shasum; it sends no telemetry
and edits no shell startup file, so it prints the `PATH` line to add when the directory is not
on `PATH`.

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

`gh repo clone` uses your GitHub login; `git clone https://github.com/KeiaiLab/SwarmOps` needs
none. `cargo install` builds in release mode, without the `test-hooks` feature, and installs both
names into `~/.cargo/bin` (add `--bin swarmops` for the long name alone); it took 20 to 36 s of
wall time on a 32-thread host. To install another commit, check it out and run the same
`cargo install`: it replaces both binaries (`Replaced package`).

### From a release

From v0.1.0 on, each release on the
[releases page](https://github.com/KeiaiLab/SwarmOps/releases) carries one archive per Linux
target, `swarmops-<version>-x86_64-unknown-linux-gnu.tar.gz` and
`swarmops-<version>-aarch64-unknown-linux-gnu.tar.gz` (`swarmops`, `so`, the license, the
third-party licenses, the README and the changelog), and a `SHA256SUMS` file to check them
against. The binaries need glibc 2.34 or newer. On glibc 2.34-2.38 hosts (Ubuntu 22.04, Debian 12,
RHEL 9) every start of the release `swarmops` and `so` binaries prints one `ld.so` line on stderr,
`weak version 'GLIBC_2.39' not found (required by …)`; exit codes, stdout envelopes and the MCP
protocol are unaffected, and on glibc 2.39 nothing is printed. [RELEASE.md](../RELEASE.md) has the
release steps.

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

## Create a store

A store is one directory. Put it on the host's local disk with room to grow: it keeps one file per
object (zstd-compressed when that is smaller), packs nothing and removes nothing yet. A new store
takes about 1 MB (its 256 object directories); SwarmOps' own history, 422 commits, took 51 MB in
8,085 files in the pilot's dry run of 2026-10-20 ([pilot.md](pilot.md)), and 57 MB in 8,854 files
at 470 commits, measured again later that day. The first export of a history then writes one
small file of derived git ids per object it exports, under `meta/derived/`, and later exports one
per object none exported before: at 470 commits the first export wrote 8,372 of them, and the
store took 92 MB in 17,232 files. Every command finds the store by `--repo` or `SWARMOPS_REPO`;
there is no default location, and `~/.local/state/swarmops/repos/<name>` is only the convention
used here.

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
- Content given as `{"file": <absolute path>}`, in `commit`, `blob_put` or `land`'s
  `resolutions`, makes the server read that path itself, with the permissions of the user it runs
  as: an agent can store any file that user can read, also one that a sandbox of the agent's own
  file access keeps from it ([SECURITY.md](../SECURITY.md)).
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
  test its change needs the files on disk some other way, such as a git worktree of the git side
  ([Building and testing a change](#building-and-testing-a-change)).

### Where the envelopes are

Every tool result is an envelope, and the server keeps none of them: `swarmops mcp` writes its
answers to the client and to nothing else, and `op log` lists the writes that succeeded, not the
calls that failed or how long they took. What [What to monitor](#what-to-monitor) reads is therefore
in the client's transcript of the session (Claude Code keeps one for each, as JSON lines under
`~/.claude/projects/`), or in a file that you have the server's answers copied to. This wrapper does
the second, one file per agent, named by its identity:

```sh
cat > ~/.local/bin/swarmops-mcp-log <<'EOF'
#!/bin/bash
# `swarmops` with a copy of every answer the server writes, in one file per agent.
dir=~/.local/state/swarmops/envelopes
install -d -m 0700 "$dir"
agent=${SWARMOPS_AGENT:-no-identity}
exec swarmops "$@" > >(tee -a "$dir/${agent//\//_}.jsonl")
EOF
chmod +x ~/.local/bin/swarmops-mcp-log
```

It takes the arguments of `swarmops` as they are, so a registration differs from the ones above in
its command only:

```json
{
  "mcpServers": {
    "swarmops": {
      "command": "/home/me/.local/bin/swarmops-mcp-log",
      "args": ["mcp", "--repo", "/home/me/.local/state/swarmops/repos/acme"],
      "env": {"SWARMOPS_AGENT": "${SWARMOPS_AGENT:-}"}
    }
  }
}
```

`claude mcp add` takes it in the same place:

```sh
claude mcp add swarmops -e SWARMOPS_AGENT=agent/worker-1 -- ~/.local/bin/swarmops-mcp-log mcp --repo ~/.local/state/swarmops/repos/acme
claude mcp list
```

```text
Added stdio MCP server swarmops with command: …/swarmops-mcp-log mcp --repo … to local config
…
swarmops: …/swarmops-mcp-log mcp --repo … - ✔ Connected
```

The wrapper `exec`s the server, so the client talks to the server itself and sees how it ends as it
would without the wrapper, the SIGABRT of one of the store's aborts included. `tee` appends each
answer to the agent's file as the server writes it: `agent/worker-1` writes
`agent_worker-1.jsonl`. A line is one JSON-RPC response. A tool call's holds the call's
envelope twice, as an object in `structuredContent` and as the text of the one content block, with
`isError` true when the envelope's `ok` is false; the answers to `initialize` and `tools/list` are
lines too, and hold no envelope. A failed `land`, cut down:

```text
{"jsonrpc":"2.0","id":6,"result":{"content":[{"type":"text","text":"{\"ok\":false,\"op\":\"land\",\"api\":1,\"error\":{\"code\":\"E_CONFLICT\",…}"}],"structuredContent":{"ok":false,"op":"land","api":1,"error":{"code":"E_CONFLICT",…},…},"isError":true}}
```

The files hold what the agents read and write, and a `read_file` line holds its page twice. They
grow for as long as the agents work and nothing prunes them: the wrapper makes their directory
`0700`, and old files are yours to move away between sessions.

## The daily loop

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
`imported` counts the commits that became changesets, and `skipped` the commits the store held
already that the import's walk met at its frontier and stopped at, not the history the store
holds (0 in a new store).
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

With nothing new, the import writes nothing. Its walk stopped at once, at the commit `main` holds:
that one commit is the `skipped` 1. Commits pushed to the trunk on top of the last promoted export
come in, and `main` moves to the last of them (`"imported":1` for one). The fetch moves the git
side's `stable` only forward, and nothing else moves it, since exports go to the review branch. When
it refuses (`! [rejected] stable -> stable (non-fast-forward)`, exit 1), the git side's `stable`
holds commits the remote lacks: an export went to it instead of the review branch, or the remote's
trunk was rewritten.

While an export waits for its review, the trunk is behind `main`, and the import fails with
`E_GIT_IMPORT_DIVERGED` although nothing is wrong: take this step after step 5, once the trunk holds
the last export ([One writer per trunk](#one-writer-per-trunk) tells this from a real divergence).

### 2. Agents commit and land

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

### 3. Look at what landed

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
back, by compare-and-swap, all or none ([README](../README.md#first-commands)).

### 4. Send it to git for review

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

### 5. Review, then move the trunk

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

### Building and testing a change

There are no workspaces yet (M5), so an agent that must build or test its change before it lands
does so in a git worktree of the git side, at the head it read, and commits the files it made there.
The script that starts the agent exports first, so that the review branch holds `main`'s head, and
takes the pair that the export prints: the commit to check out, and the changeset it is.

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
checked out. The agent edits, builds and tests in it with whatever the project uses, here its
`check.sh`:

```sh
(cd ~/work/worker-1 && ./check.sh)
```

```text
check: ok
```

Then it commits the files as they are on disk: one `put` per changed file, each named by its
absolute path, which the server reads itself (see above), with `parent` the changeset the worktree
is. The `commit` tool takes these arguments, and `land` the changeset it returns as its `head`:

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

- `parent` is the changeset the worktree stands for, which is `main`'s head only if nothing landed
  since the export. `land` merges the change onto the head it finds, three-way, as it does for any
  commit. What ran in the worktree is the base, not that merge: CI on the review branch tests the
  merge.
- A `put` writes a regular file unless it says `"mode": "exec"` (or `"symlink"`): name the mode of
  every script, or the commit drops its executable bit.
- `worktree remove` needs `--force` here, because git sees the agent's edits as uncommitted. A
  killed agent leaves its worktree registered in the git side: `git -C "$GIT_SIDE" worktree list`
  shows it, as `prunable` once its directory is gone, and `worktree prune` forgets it.

### One writer per trunk

Between an import and the next move of the trunk (step 5), only one side may change the trunk, the
store (through the review branch) or git. People change it in one of three ways that keep to this:

- through SwarmOps, with an identity of their own, as an agent does; the review covers their changes
  too;
- on the trunk in git, after step 5 and before an agent lands again, which step 1 then takes in;
- on a branch, which you import into a ref of its own and land:

```sh
git -C "$GIT_SIDE" fetch origin feature/farewell:feature/farewell
swarmops git import "$GIT_SIDE" --ref feature/farewell --into release/farewell
swarmops land release/farewell --key "$(uuidgen)"
swarmops ref delete release/farewell --expect "$(swarmops resolve release/farewell | field result.id)"
```

The land squashes the branch's commits into one changeset on `main`, whose author is the author of
the branch's last commit and whose committer is you; the next export takes it to the review branch,
and after the review the push of step 5 takes it to the trunk as a fast-forward:

```sh
swarmops git export "$GIT_SIDE" --ref main --branch review
git -C "$GIT_SIDE" push origin review
git -C "$GIT_SIDE" log -1 --format="%an <%ae> / %cn <%ce>%n%B" review
```

```text
{"ok":true,"op":"git_export","api":1,"result":{"exported":1,"objects_written":1,"head_changeset":"813dcf7f…","head_sha":"c19cdeac0daf33835ee6c3d470724450e4229a30","branch":"review","old_sha":"7073a6601acef10a0091e2b7737916399735e721","branch_moved":true,"incremental":true,"rewritten":0},…}
To …
   0eb07c3..c19cdea  review -> review
Ada Lovelace <ada@example.com> / operator/ada <ada@operator.swarmops.local>
farewell: say goodbye

SwarmOps-Changeset: 813dcf7f16416001a5d5ef4815cf9d4c073250d76403db6ed4add70b19bce83f
SwarmOps-Land-Key: a3921b5a-d809-4003-ad53-b77894dcfb22
SwarmOps-Squashed-From: c2ec3b59e9030b9a5c0ff8c0bea98f546e43d45e8a057c2568d9e5255eba469f
```

When both sides moved anyway (here an agent landed a change and exported it, and before the review
ended a person pushed one to the trunk), nothing is merged: the push of step 5 is rejected as no
fast-forward, and step 1's import fails with `E_GIT_IMPORT_DIVERGED` (exit 3) and writes nothing:

```sh
git -C "$GIT_SIDE" push origin review:stable
git -C "$GIT_SIDE" fetch origin stable:stable
swarmops git import "$GIT_SIDE" --ref stable --into main --pretty
```

```text
To …
 ! [rejected]        review -> stable (fetch first)
error: failed to push some refs to '…'
hint: Updates were rejected because the remote contains work that you do not
…
From …
   7073a66..9745112  stable     -> stable
{
  "ok": false,
  "op": "git_import",
  "api": 1,
  "error": {
    "code": "E_GIT_IMPORT_DIVERGED",
    "message": "main has diverged from the git history: its head 3d264e8e03aa34acbb82049c1b6d906c418fe35839e22b33d359b56e8b326e4e stands for git commit d9d94398b96c722eb2afab2148198c0fd17a2d5b, which the tip 9745112402b9a55c434417c1d9228ba4d3f78985 does not reach",
    "retryable": false,
    "exit": 3,
    "details": {
      "base": "d9d94398b96c722eb2afab2148198c0fd17a2d5b",
      "git_head": "9745112402b9a55c434417c1d9228ba4d3f78985",
      "target_head": "3d264e8e03aa34acbb82049c1b6d906c418fe35839e22b33d359b56e8b326e4e",
      "target_ref": "main"
    },
    "hint": "swarmops log main"
  },
  …
}
```

The import fails the same way while an export merely waits for its review, so look at what the trunk
has that the review branch lacks: nothing if the trunk only lags, and here the person's commit:

```sh
git -C "$GIT_SIDE" log --oneline review..stable
```

```text
9745112 release: 1.0.2
```

To keep git's history as it is, with no forced push to the trunk, put the store's new work on top of
git's: import the trunk into a ref of its own, land `main`'s changes onto it (`land` starts from the
changeset both sides share, the merge base of the two), move `main` there, and export. The review
branch still holds the export of the old line, and `main` no longer descends from it, so the export
refuses (`E_GIT_EXPORT_DIVERGED`, reason `source_moved_back`) until it is forced, and the push is
forced too: the branch is a proposal that nothing builds on, and the new line goes through the
review again.

```sh
swarmops git import "$GIT_SIDE" --ref stable --into release/git
HEAD_WAS=$(swarmops resolve main | field result.id)
LANDED=$(swarmops land main --target release/git --key "$(uuidgen)" | field result.landed)
swarmops ref set main "$LANDED" --expect "$HEAD_WAS"
swarmops git export "$GIT_SIDE" --ref main --branch review --force
git -C "$GIT_SIDE" push --force origin review
swarmops ref delete release/git --expect "$LANDED"
```

```text
{"ok":true,"op":"git_export","api":1,"result":{"exported":1,"objects_written":2,"head_changeset":"c250bd46…","head_sha":"6f4295e40f9f8db8449ff18cc036637abb4dac8d","branch":"review","old_sha":"d9d94398b96c722eb2afab2148198c0fd17a2d5b","branch_moved":true,"incremental":true,"rewritten":0,"warnings":["the branch review had diverged (source_moved_back), and the export moved it all the same"]},…}
To …
 + d9d9439...6f4295e review -> review (forced update)
```

That land squashes all of `main`'s changes since the shared changeset into one changeset. The other
way is to give up the commits made in git: a forced push of the review branch to the trunk
(`git push --force origin review:stable`, and `git fetch origin +stable:stable` after it) replaces
the trunk with the store's work, and the commits made in git are gone from it.

An export that cannot continue its branch fails with `E_GIT_EXPORT_DIVERGED` and a reason, each with
its hint: `both_moved` (git and `main` both moved past the branch's last export, and nothing is
merged), `branch_ahead` (commits made in git on top of the export: an import of the branch takes
them in), `branch_moved` (the branch names no export of this history), `source_moved_back` (`main`
moved back, as `op log` shows), `lossy_history` (the branch holds the commits a lossy import was
made of: export to another branch) and `moved_during_export` (run it again). `--force` moves the
branch all the same, with a warning. An export that finds `refs/heads/<branch>.lock` in the git
side, which a git process holds or a dead one left, fails with `E_GIT_WRITE` (`ref_locked`) until
the file is removed, once no git process runs.

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
SwarmOps' own history (7,647 objects) they took 0.1 s and 0.3 s in the pilot's dry run of
2026-10-20, and 0.09 s and 0.23 s for 8,368 objects, measured again later that day at a load
average of about 20. A store with damage fails with
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
- **Every journal, index and ref write holds `meta/lock`**, an `flock`. Object writes take no lock,
  but no frame names an object before the object is durable; derived values and external id entries
  take none either, and are caches that no frame names. Readers take no lock.
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
what was exported after the backup, from the review branch, which holds every export (the trunk
holds those that were promoted):

```sh
mv "$SWARMOPS_REPO" "$SWARMOPS_REPO.damaged"
cp -a "$BACKUP" "$SWARMOPS_REPO"
swarmops verify --deep
swarmops git import "$GIT_SIDE" --ref review --into main
swarmops git export "$GIT_SIDE" --ref main --branch review
```

```text
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":42,"changesets_checked":14,"problems":[],"findings":[]},…}
{"ok":true,"op":"git_import","api":1,"result":{"imported":1,"skipped":1,"head_changeset":"ebbd2134…","git_head_sha":"327bbc8898086c815a71e9ca1ab69bec147e5e36","target_ref":"main","ref_moved":true,"old_head":"c250bd46…","lossy":[],"lossy_total":0},…}
{"ok":true,"op":"git_export","api":1,"result":{"exported":0,"objects_written":0,…,"branch_moved":false,"incremental":true,"rewritten":0},…}
```

The land exported after the backup comes back as the commit it was exported as, and the export
finds nothing to write. It is an imported changeset now: `log` shows its agent as
`unknown/unknown`, and its signatures keep the git author, the agent that made it. What is lost is
what was written after the backup and never exported: changesets no export reached (an agent's
unlanded commits, lands made since the last export), ref moves, and the operation log's entries.

The git side is itself a copy of everything exported. Without any backup, a new store made from it
(`swarmops init`, then `swarmops git import` of each branch) exports to the same commits.

## Making room

A store only grows ([Limits](#limits)): nothing is collected until M6's `swarmops gc`. Two kinds of
file in it may go by hand meanwhile, with every writer stopped. This is what the storage spec gives
for each, and no more:

- **The leftovers of interrupted writes**, files in `objects/tmp/` and `meta/derived/tmp/`. A write
  stages its file there and links it into place, and a writer that dies on the way leaves the file.
  The spec has a crash before the frame that names an object leave only orphan objects and temp
  files, and no frame names an object before it is linked and durable (§2, rules 1 and 2), so a
  staging file is never the only name of an object that a changeset needs. `swarmops gc` is to clear
  the files of `objects/tmp/` that are older than an hour (§2) and the temp files of `meta/derived/`
  (§3). By hand, remove the files older than an hour, and not the directories: the spec lists them
  as part of the layout, and does not say that a store makes them again.
- **The derived values**, `meta/derived/`, which may go whole. They are a cache that neither the
  journal nor any other file names (§3), and removing a value's file, or `meta/derived/`, makes the
  value derived again (§5).

The orphan objects stay: telling them from the objects a changeset names takes the mark and sweep
from the journal that `gc` gets later. The objects, the journal, the index and the refs stay as they
are.

Both removals need the store to themselves. A writer is every process that can write it: the agents'
sessions and their MCP servers (a server lives as long as its session), the loop's commands and
`swarmops derive`, and also a read, which stores the values it derives. `flock` on `meta/lock` stops
none of them, since objects and derived values are written without it. Check that no `swarmops` is
left (`so` is the same program):

```sh
pgrep -ax 'swarmops|so'
```

```text
1503 swarmops mcp --repo …
```

That is an MCP server that a client started for its session: end the session. With nothing left, the
command prints nothing and exits 1. Then, from the store's directory (the `:?` refuses an empty
variable, which `cd` would take for the current directory), the leftovers (on a store that never
derived a value, `find` says that `meta/derived/tmp` does not exist, which is right):

```sh
cd "${SWARMOPS_REPO:?}"
find objects/tmp meta/derived/tmp -type f -mmin +60
```

```text
objects/tmp/.tmpUbSpKg
meta/derived/tmp/3849408-0-ffc434ac1bd4cb92
```

This store had three leftovers of writers that were killed in the middle of a write; two were set
back a day with `touch -d`, to have the hour pass, and the third is as young as it was. The removal
leaves that one:

```sh
find objects/tmp meta/derived/tmp -type f -mmin +60 -delete
find objects/tmp meta/derived/tmp -type f
```

```text
objects/tmp/.tmpoyk2fg
```

The derived values come off whole, and `verify --deep` has no stored value left to check:

```sh
du -sh meta/derived; find meta/derived -type f | wc -l
rm -rf meta/derived
swarmops verify --deep
```

```text
344K	meta/derived
38
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":45,"changesets_checked":15,"problems":[],"findings":[]},"meta":{"repo":"…","elapsed_ms":2,…}}
```

A read derives the values it needs and stores them in a `meta/derived/` that it makes again,
`swarmops derive` makes the eager ones for every changeset at once, and `verify --deep` then checks
the stored values of the built-in derivations against the changesets (§5):

```sh
swarmops history main:src/lib.rs --limit 1 --human
find meta/derived -type f | wc -l
swarmops derive
swarmops verify --deep
```

```text
path: src/lib.rs
entries[0].changeset: 1092352e3a895f0ae9301a5e0637b74ae36763b57d3e907897e0e0408a07830b
…
2
{"ok":true,"op":"derive","api":1,"result":{"derived":15,"cached":0,"next":20},"meta":{"repo":"…","elapsed_ms":1,…}}
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":45,"changesets_checked":15,"problems":[],"findings":[]},"meta":{"repo":"…","elapsed_ms":2,…}}
```

At the size of the pilot's store, SwarmOps' own 470 commits with a change landed on top, removing
`meta/derived/` took it from 92 MB in 17,230 files to 57 MB in 8,859 files (measured on 2026-10-20).
An export into an empty git directory stored its 8,371 values again and wrote the same head; an
export to the branch that already held everything wrote nothing and still stored 2,544 of them. So
the values trade room for the time a later export spends deriving them, and the objects, most of a
store, stay.

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
- **Creating a store again** goes through git: export every ref you keep (`main` to the review
  branch, each `release/<name>` to a branch of its own), make the new store with the new binary, and
  import each branch into its ref. The new store exports to the same commits. What does not carry
  over: changesets no ref reaches, the operation log (and so `undo`), and the changeset ids of the
  changesets made in the store, which come back as imported changesets with ids of their own.

```sh
NEW=~/.local/state/swarmops/repos/acme-next
swarmops init "$NEW" --name acme
swarmops --repo "$NEW" git import "$GIT_SIDE" --ref review --into main
swarmops --repo "$NEW" git export "$GIT_SIDE" --ref main --branch review
```

```text
{"ok":true,"op":"repo_init","api":1,"result":{"repo_id":"393244461d7a5f01f7d600c8612a93f0","name":"acme","store_version":1,"changesets":0,"refs":0},…}
{"ok":true,"op":"git_import","api":1,"result":{"imported":10,"skipped":0,"head_changeset":"b9607b5d…","git_head_sha":"327bbc8898086c815a71e9ca1ab69bec147e5e36","target_ref":"main","ref_moved":true,"lossy":[],"lossy_total":0},…}
{"ok":true,"op":"git_export","api":1,"result":{"exported":0,"objects_written":0,…,"head_sha":"327bbc8898086c815a71e9ca1ab69bec147e5e36","branch":"review",…,"branch_moved":false,…,"rewritten":0},…}
```

The hash contract (the byte layouts of blobs, trees and changesets, and the hash recipe) never
changes: a changeset's id is the same under every `FORMAT`.

## Limits

The ones an operator meets first; [known-limitations.md](known-limitations.md) lists them all,
each with the milestone that takes it.

- **One host, one writer per trunk.** No sync between hosts until M11; git carries history between
  them, with one host landing and exporting and the others importing.
- **No workspaces** (M5): agents read and write through the tools, a whole file per change. An agent
  that must build first works in a git worktree
  ([Building and testing a change](#building-and-testing-a-change)).
- **No review** (M7): SwarmOps does not hold a landing for a person's approval. The loop puts the
  review in git, between the review branch and the trunk ([step 5](#5-review-then-move-the-trunk)).
- **Nothing is collected** (M6): `objects/tmp/` keeps the leftovers of interrupted writes, and
  `meta/derived/` every derived value. A store only grows, but for what [Making room](#making-room)
  removes by hand.
- **No repair tool** (M6): `verify` reports damage, and a restore fixes it.
- **Writers take turns.** Every write holds one store-wide lock across two journal syncs: about
  21 ms per uncontended commit on the maintainers' host. With 32 writers committing in a loop, a
  commit can wait a few hundred transactions; on a slow enough disk that passes the 10 s wait for
  the lock, and the commit fails with `E_BUSY`.
- **Exports write loose objects**, one file and one fsync per object, until `git gc` packs them:
  in the pilot's dry runs of 2026-10-20, exporting SwarmOps' history into an empty repository
  wrote 7,651 objects in 12 s on a busy host (a load average of 150 to 500) and in 6.1 s at 80 to
  130; measured again later that day, 8,372 objects took 4.0 s at a load of about 20. An export
  that dies while it moves the branch leaves `refs/heads/<branch>.lock`, and an export leaves no
  reflog entry.
- **Content git can hold and a changeset cannot** (submodules, names that are not UTF-8, odd modes
  and tree orders) is refused by the import unless `--lossy` allows it, and a history imported with
  a loss exports as new commits.

## What to monitor

Every command and every tool result is an envelope, so a log of envelopes is the monitoring data. A
command prints its envelope; the MCP server keeps none of its answers, so what agents' calls did is
read from the files that [the wrapper](#where-the-envelopes-are) keeps, one per agent, or from the
client's transcripts. A line of those files holds its envelope twice, as an object and, escaped, as
text; these commands read the object, which has no escaped quotes, and need no `jq`. In the
wrapper's directory:

```sh
cd ~/.local/state/swarmops/envelopes
for f in *.jsonl; do
  printf '%-22s calls %s, failed %s, recovered %s, warnings %s\n' "${f%.jsonl}" \
    "$(grep -c '"structuredContent"' "$f")" "$(grep -c '"isError":true' "$f")" \
    "$(grep -c '"recovered"' "$f")" "$(grep -c '"warnings":' "$f")"
done
cat *.jsonl | grep -o '"code":"E_[A-Z_]*"' | sort | uniq -c | sort -rn
cat *.jsonl | grep -o '"elapsed_ms":[0-9]*' | cut -d: -f2 | sort -n | tail -3
```

```text
agent_worker-1         calls 7, failed 1, recovered 0, warnings 0
agent_worker-2         calls 6, failed 2, recovered 0, warnings 0
      1 "code":"E_PATH_NOT_FOUND"
      1 "code":"E_CONFLICT"
      1 "code":"E_CAS_MISMATCH"
19
23
31
```

The first command counts, for each agent, the calls answered, the failed ones, the writes that
repaired what a dead writer left (`meta.recovered`) and the results that carry `warnings`; the
second counts the failures by code; the third lists the three slowest calls by `meta.elapsed_ms`.
What to read in them:

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
  `meta/derived/`, which only grow. `meta/derived/` grows with the exports: the first export of a
  history writes one file of derived git ids per object it exports, about as many files as
  `objects/` holds, each taking a whole filesystem block (4 KiB on ext4) for its 72 or 73 bytes. At
  470 commits the store took 57 MB in 8,854 files after the import, and 92 MB in 17,232 files after
  the first export.

  The store keeps one file per object and one per derived value, so count its files and the
  filesystem's inodes as well: ext4 fixes the number of inodes when the filesystem is made, and a
  store of small files can use them up before it uses the space. [Making room](#making-room) says
  what may go.

  ```sh
  find "$SWARMOPS_REPO" -type f | wc -l
  df --output=iused,iavail,ipcent "$SWARMOPS_REPO"
  ```

  ```text
  91
    IUsed     IFree IUse%
  3021323 241161717    2%
  ```
- **The git side**: each export's `exported`, `branch_moved` and `rewritten` (0 unless the history
  was imported with a loss), any `E_GIT_IMPORT_DIVERGED` or `E_GIT_EXPORT_DIVERGED` (someone wrote
  the trunk, or an export waits for its review) or `E_GIT_WRITE`, how long the review branch has
  been ahead of the trunk (`git -C "$GIT_SIDE" log --oneline stable..review` lists what waits), and
  the CI runs on the review branch and the deployment runs on the trunk.
- **`op log`**: who moved which ref, for moves no one expected.
