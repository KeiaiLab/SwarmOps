# Troubleshooting

This page goes from what you see to why it happens and what to run. It covers the installer, the
MCP server, where a store may live, a busy store, a damaged store, git import and export,
workspaces, and every `E_*` error code that a command or a tool call can return.

Every example was run on 2026-10-22: the commands with the binary built from this repository
(`swarmops 0.1.0`), in a scratch directory that holds the store `acme` (`SWARMOPS_REPO=acme`); the
installer from get.swarmops.cc; and Claude Code's `claude mcp` with a configuration directory of
its own. Every output is what it printed, with three cuts: an id is cut to its first 8 hex
digits and `…`, the scratch directory in an absolute path is `…`, and `…` alone marks a cut of a
long output. The examples run the long name, `swarmops`; `so` is the same program.

## Read the error first

Every command prints one JSON envelope on stdout, and every MCP tool result holds the same
envelope. A failure has `"ok": false` and an `error` object:

| Field | What it tells you |
| --- | --- |
| `code` | The stable `E_*` code. The [table below](#every-error-code) links each one to its section. |
| `exit` | The process exit code: 1 failed, 2 malformed request, 3 conflict, 4 retryable, 5 identity, 6 not found. |
| `retryable` | `true` when running the operation again can succeed: as it is, or after re-reading what the error names. |
| `details` | Machine-readable context: the path, the ref, what the ref holds now. A store file is named relative to the store's root, never by a host path. |
| `hint` | The next command to run, when there is an obvious one; else `null`. |

So the first step is the same for every failure: read `hint`, then `details`. Two commands help
with any of them:

```sh
swarmops schema OpError    # the error object's schema
swarmops verify            # is the store itself sound?
```

The README's [Output and exit codes](../README.md#output-and-exit-codes) says more about the
envelope. A run that meets one of the store's four documented aborts prints no envelope at all:
the process aborts, and says why on stderr ([The server exits](#the-server-exits-during-a-session)).

## Every error code

`ErrorCode` in `crates/swarmops-api/src/error.rs` is the one table of codes, exit codes and
retry advice; this is that table. Four codes are in the schema but no operation raises them yet.

| Code | Exit | Retryable | Means | Section |
| --- | --- | --- | --- | --- |
| `E_EXISTS` | 1 | no | `init` found a store there already. | [Where a store may live](#e_exists) |
| `E_NOT_A_REPO` | 6 | no | No store at that path, or no store named. | [Where a store may live](#e_not_a_repo) |
| `E_UNSUPPORTED_FS` | 1 | no | The store's path leads under `/mnt/`. | [Where a store may live](#e_unsupported_fs) |
| `E_IDENTITY_REQUIRED` | 5 | no | A write without an identity. | [Identity](#e_identity_required) |
| `E_INVALID_ARG` | 2 | no | A malformed argument, request or command line. | [Arguments and revisions](#e_invalid_arg) |
| `E_REV_NOT_FOUND` | 6 | no | A revision names no changeset. | [Arguments and revisions](#e_rev_not_found) |
| `E_AMBIGUOUS_ID` | 1 | no | An id prefix matches more than one changeset. | [Arguments and revisions](#e_ambiguous_id) |
| `E_PATH_NOT_FOUND` | 6 | no | Nothing is at that path at that revision. | [Arguments and revisions](#e_path_not_found) |
| `E_IS_DIR` | 1 | no | A file read named a directory. | [Arguments and revisions](#e_is_dir) |
| `E_REF_NAME_INVALID` | 1 | no | A ref name that is not `main` or `release/<name>`. | [Arguments and revisions](#e_ref_name_invalid) |
| `E_OP_NOT_FOUND` | 6 | no | No operation by that number. | [Arguments and revisions](#e_op_not_found) |
| `E_EMPTY_COMMIT` | 1 | no | The change list changes nothing. | [Commits](#e_empty_commit) |
| `E_TYPE_CONFLICT` | 1 | no | A file and a directory at the same path. | [Commits](#e_type_conflict) |
| `E_TOO_LARGE` | 1 | no | Content past a size limit. | [Commits](#e_too_large) |
| `E_UNKNOWN_PARENT` | 6 | no | The parent left the store before the commit recorded. | [Commits](#e_unknown_parent) |
| `E_CAS_MISMATCH` | 4 | yes | The ref no longer holds what you expected. | [Commits](#e_cas_mismatch) |
| `E_CONFLICT` | 3 | no | The land conflicts with the target; nothing recorded. | [Landing and undo](#e_conflict) |
| `E_NOTHING_TO_LAND` | 1 | no | The target holds the change already. | [Landing and undo](#e_nothing_to_land) |
| `E_CAS_RETRY_EXHAUSTED` | 4 | yes | The target moved before every land attempt. | [Landing and undo](#e_cas_retry_exhausted) |
| `E_NOTHING_TO_UNDO` | 1 | no | The operation moved no ref. | [Landing and undo](#e_nothing_to_undo) |
| `E_BUSY` | 4 | yes | Another writer held the store's lock for the whole 10 s wait. | [A busy store](#e_busy) |
| `E_STORE_CORRUPT` | 1 | no | Stored data failed verification. | [A damaged store](#e_store_corrupt) |
| `E_IO` | 1 | no | The filesystem refused an operation. | [A damaged store](#e_io) |
| `E_IO_TRANSIENT` | 4 | yes | A journal append failed and was cut back; nothing committed. | [A damaged store](#e_io_transient) |
| `E_INTERNAL` | 1 | no | A bug in SwarmOps. | [A damaged store](#e_internal) |
| `E_GIT_NOT_A_REPO` | 6 | no | No git repository at that path. | [Git import](#e_git_not_a_repo) |
| `E_GIT_REF_NOT_FOUND` | 6 | no | The git ref names no commit. | [Git import](#e_git_ref_not_found) |
| `E_GIT_UNSUPPORTED` | 1 | no | A repository the import does not read (shallow, SHA-256, …). | [Git import](#e_git_unsupported) |
| `E_GIT_READ` | 1 | no | An object is missing or damaged in the git repository. | [Git import](#e_git_read) |
| `E_GIT_IMPORT_LOSSY` | 1 | no | Content a changeset cannot hold as git has it. | [Git import](#e_git_import_lossy) |
| `E_GIT_IMPORT_DIVERGED` | 3 | no | The store's ref and the git history went separate ways. | [Git import](#e_git_import_diverged) |
| `E_GIT_EXPORT_DIVERGED` | 3 | no | The git branch holds commits that are no export of the ref. | [Git export](#e_git_export_diverged) |
| `E_GIT_WRITE` | 1 | no | The git repository could not be written. | [Git export](#e_git_write) |
| `E_WS_EXISTS` | 1 | no | A workspace of that name exists, made with other arguments. | [Workspaces](#e_ws_exists) |
| `E_WS_NOT_FOUND` | 6 | no | No workspace by that name, or around that directory. | [Workspaces](#e_ws_not_found) |
| `E_WS_LOCKED` | 4 | yes | Another mutation held the workspace's lock for the whole 5 s wait. | [Workspaces](#e_ws_locked) |
| `E_WS_DIRTY` | 1 | no | The workspace holds edits the operation would overwrite or lose. | [Workspaces](#e_ws_dirty) |
| `E_PATH_NOT_EMPTY` | 1 | no | `ws create` was given a directory that holds something. | [Workspaces](#e_path_not_empty) |
| `E_UNRESOLVED_CONFLICTS` | 1 | no | A rebase still has conflicting paths no one resolved. | [Workspaces](#e_unresolved_conflicts) |
| `E_MARKERS_PRESENT` | 1 | no | `ws resolve` found a conflict marker line in a file. | [Workspaces](#e_markers_present) |
| `E_WS_UNPORTABLE` | 1 | no | The tree holds paths the workspace's volume cannot hold. | [Workspaces](#e_ws_unportable) |
| `E_CHECK_FAILED` | 1 | no | A workspace's check or bootstrap command failed or timed out. | [Workspaces](#e_check_failed) |
| `E_REF_NOT_FOUND` | 6 | no | Not raised yet. | [Codes not raised yet](#codes-not-raised-yet) |
| `E_AUTH` | 5 | no | `swarmops serve` names no caller, or no grant of the store's `access.toml` names it. | [Serve over ssh](operate/serve-over-ssh.md#refusals-a-client-sees) |
| `E_POLICY_DENIED` | 5 | no | The store's `access.toml` grants the caller no such action, or the file is invalid. | [Serve over ssh](operate/serve-over-ssh.md#refusals-a-client-sees) |
| `E_MAINTENANCE_BUSY` | 4 | yes | Not raised yet. | [Codes not raised yet](#codes-not-raised-yet) |

`git import` and `git export` are commands only, so the `E_GIT_*` codes never come from a tool
call; `init` is a command only too. Every other code can come from both.

## The installer

`curl -fsSL https://get.swarmops.cc/install.sh | bash` runs `scripts/install.sh`. It prints its
progress and its errors on stderr, each line starting with `swarmops-install:`, and stops with
status 1 on a failure and 2 on a usage error. It checks the host before it downloads anything, and
installs nothing when a check after the download fails. Options go after `bash -s --`;
`bash install.sh --help` lists them.

A good run, for reference (into a directory not on `PATH`):

```sh
curl -fsSL https://get.swarmops.cc/install.sh | bash -s -- --version 0.1.0 --dir inst
```

```text
swarmops-install: Installing SwarmOps 0.1.0 (x86_64-unknown-linux-gnu) into inst.
swarmops-install: Downloading https://github.com/KeiaiLab/SwarmOps/releases/download/v0.1.0/swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz
swarmops-install: swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz matches SHA256SUMS.
swarmops 0.1.0
Installed swarmops and so (its short form) in …/inst.
…/inst is not on PATH. Add this line to your shell startup file (~/.profile, ~/.bashrc or ~/.zshrc):
  export PATH="…/inst:$PATH"
Quick start: https://github.com/KeiaiLab/SwarmOps#quick-start
Website: https://swarmops.cc
```

### The installer stops

Each message below is one the script prints; `X`, `V` and `DIR` stand for what it names.

| Message | Cause | What to do |
| --- | --- | --- |
| `Unknown option 'X'; see --help.` (exit 2) | A misspelt option. | `… \| bash -s -- --version 0.1.0` |
| `X needs a value; see --help.` (exit 2) | `--version` or `--dir` last on the line. | Give the value: `--dir ~/bin`. |
| `SwarmOps runs on Linux today; macOS and Windows are next.` then `This system is X.` | Not Linux. | No release build runs there yet. |
| `No release build runs on X, only on x86_64 and aarch64.` | Another CPU architecture. | Build from source (the README's [Install](../README.md#install)). |
| `This system's C library is musl; …` | Alpine and other musl systems. | Build from source. |
| `This system has glibc V; the release binaries need 2.34 or newer.` | An older distribution. | Build from source, or a newer host. |
| `Cannot tell this system's glibc version …` | Neither `ldd --version` nor `getconf GNU_LIBC_VERSION` gave one. | Build from source. |
| `Neither curl nor wget is installed: …` | No downloader. | Install one, then run it again. |
| `'V' is not a release version: X.Y.Z or X.Y.Z-pre, such as 0.1.0.` | `--version 0.1` or another short form. | Give all three numbers; a leading `v` is fine. |
| `Cannot find the latest release: …` | The redirect from the releases page did not name a tag (network, proxy). | Pin `--version`, or check that `https://github.com/KeiaiLab/SwarmOps/releases/latest` opens. |
| `Cannot download …: is V a release? …` | No such release, or the download failed. | Check the [releases page](https://github.com/KeiaiLab/SwarmOps/releases). |
| `Cannot download …/SHA256SUMS.` | The checksum file did not download. | Run it again. |
| `SHA256SUMS of vV has no line for …: nothing was installed.` | The release has no archive for this target. | Pick another release. |
| `Neither sha256sum nor shasum is installed, …` | The download cannot be checked. | Install coreutils or perl's `shasum`. |
| `… does not match its line in SHA256SUMS: nothing was installed.` | A damaged or altered download. | Run it again; if it repeats, report it ([SECURITY.md](../SECURITY.md)). |
| `Cannot write in DIR.` | DIR belongs to another user, such as `/usr/local/bin` for a normal user. | Run as root (`… \| sudo bash`), or pick a DIR you own. |
| `DIR/swarmops is a directory: nothing was installed.` | Something else holds the name. | Move it, or pick another DIR. |
| `Installed, but 'DIR/swarmops --version' failed.` | The binary does not start here. | Run `DIR/swarmops --version` to see why. |
| `DIR holds no SwarmOps binary: nothing was removed.` | `--uninstall` with the wrong DIR. | Give the DIR you installed into, with `--dir`. |
| `HOME is not set: name the install directory with --dir.` | A bare environment, such as some containers. | `--dir /usr/local/bin` as root, or another DIR. |

Three of these, as they were printed (`install.sh` is a downloaded copy of the script):

```sh
curl -fsSL https://get.swarmops.cc/install.sh | bash -s -- --frob
bash install.sh --version 9.9.9 --dir inst
bash install.sh --version 0.1.0 --dir /usr/local/bin
```

```text
swarmops-install: Unknown option '--frob'; see --help.
swarmops-install: Installing SwarmOps 9.9.9 (x86_64-unknown-linux-gnu) into inst.
swarmops-install: Downloading https://github.com/KeiaiLab/SwarmOps/releases/download/v9.9.9/swarmops-9.9.9-x86_64-unknown-linux-gnu.tar.gz
curl: (22) The requested URL returned error: 404
swarmops-install: Cannot download https://github.com/KeiaiLab/SwarmOps/releases/download/v9.9.9/swarmops-9.9.9-x86_64-unknown-linux-gnu.tar.gz: is 9.9.9 a release? https://github.com/KeiaiLab/SwarmOps/releases
swarmops-install: Installing SwarmOps 0.1.0 (x86_64-unknown-linux-gnu) into /usr/local/bin.
swarmops-install: Downloading https://github.com/KeiaiLab/SwarmOps/releases/download/v0.1.0/swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz
swarmops-install: swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz matches SHA256SUMS.
mktemp: failed to create file via template '/usr/local/bin/.swarmops.XXXXXX': Permission denied
swarmops-install: Cannot write in /usr/local/bin.
```

An interrupted run leaves the old binaries in place: each one is copied under a temporary name in
DIR and renamed over its own name, and the temporary files go when the script ends.

### `swarmops: command not found` after the install

The installer edits no shell startup file. When DIR is not on `PATH` it prints the line to add,
as in the good run above (`fish_add_path "DIR"` under fish). Add it, then open a new shell. Under
the default, DIR is `~/.local/bin` for a normal user and `/usr/local/bin` for root.

### A `weak version 'GLIBC_2.39' not found` line on every start

On glibc 2.34 to 2.38 (Ubuntu 22.04, Debian 12, RHEL 9), every start of the release binaries
prints this one `ld.so` line on stderr, and the installer says so before it downloads. Exit codes,
stdout and the MCP protocol are unaffected, so nothing needs fixing. On glibc 2.39 nothing is
printed.

### Update and uninstall

Run the same command again to update: it says `Replaced swarmops <version>.`. `--uninstall`
removes `swarmops` and `so` from DIR when they are SwarmOps' own, and leaves every store alone:

```text
Removed swarmops so from inst. Stores are left as they are.
```

## The MCP server

`swarmops mcp` is started by the client (Claude Code), not by you, so its failures show in the
client. `claude mcp list` starts each approved server once and reports on it.

### `claude mcp list` says `Failed to connect`

Three servers registered in one project, one good, one with a typo in the command, one with a
flag `mcp` does not take:

```sh
claude mcp add swarmops -e SWARMOPS_AGENT=agent/worker-1 -- swarmops mcp --repo …/acme
claude mcp add typo -- swarmopz mcp --repo …/acme
claude mcp add refused -- swarmops mcp --repo …/acme --frob
claude mcp list
```

```text
Checking MCP server health…

swarmops: swarmops mcp --repo …/acme - ✔ Connected
typo: swarmopz mcp --repo …/acme - ✘ Failed to connect — ENOENT: Executable not found in $PATH: "swarmopz"
refused: swarmops mcp --repo …/acme --frob - ✘ Failed to connect — CONNECTION_CLOSED: Connection closed
```

- **`ENOENT: Executable not found in $PATH`**: the client cannot find the program. Check the name,
  run `command -v swarmops`, or register the binary by its absolute path, in the directory the
  installer named (`~/.local/bin/swarmops` by default).
- **`CONNECTION_CLOSED`**: the program started and exited at once. A command line that parses as
  `swarmops mcp` but is refused starts no server: its envelope goes to stderr, since stdout belongs
  to the protocol. Run the registered command yourself to read it:

```sh
swarmops mcp --repo "$PWD/acme" --frob < /dev/null
```

```text
{"ok":false,"op":"mcp","api":1,"error":{"code":"E_INVALID_ARG","message":"unexpected argument '--frob' found","retryable":false,"exit":2,"details":{},"hint":"Usage: swarmops mcp --repo <STORE>\nFor more information, try '--help'."},"meta":{"elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

  That line is on stderr, and the exit code is 2. A `SWARMOPS_FORMAT` that is neither `json` nor
  `human` is refused the same way (`SWARMOPS_FORMAT: "yaml" is not a format: json or human`). A
  usage error before the word `mcp` (`swarmops --frob mcp`) is an ordinary command error, printed on
  stdout with exit code 2; no server starts either.

A store that does not exist does not stop the server: it starts and serves, and every call then
fails with `E_NOT_A_REPO` (below).

### `⏸ Pending approval`

A server named in a project's `.mcp.json` is not started until you approve it once in that project:
`claude mcp list` shows `` ⏸ Pending approval (run `claude` to approve) ``. Start `claude` in the
project and approve it.

### `Missing environment variables: SWARMOPS_AGENT`

This `.mcp.json` passes `"SWARMOPS_AGENT": "${SWARMOPS_AGENT}"`, without a default, in a shell
where the variable is not set:

```text
Checking MCP server health…

swarmops: swarmops mcp --repo …/acme - ⏸ Pending approval (run `claude` to approve)

MCP config diagnostics ⚠

For help configuring MCP servers, see: https://code.claude.com/docs/en/mcp

[Contains warnings] Project config (shared via .mcp.json)
Location: …/proj/.mcp.json
 └ [Warning] [swarmops] mcpServers.swarmops: Missing environment variables: SWARMOPS_AGENT
```

Claude Code then passes the text `${SWARMOPS_AGENT}` on to the server as it is (the README's
[Register the server](../README.md#register-the-server) says so). A server started with that value
serves reads as usual, but every write fails, because that text is no agent name (the text of a
`commit` tool result):

```text
{"ok":false,"op":"commit","api":1,"error":{"code":"E_INVALID_ARG","message":"SWARMOPS_AGENT: invalid agent name \"${SWARMOPS_AGENT}\": expected <family>/<instance> of at most 255 bytes","retryable":false,"exit":2,"details":{"source":"SWARMOPS_AGENT"},"hint":"agent names look like agent/worker-17: family [a-z0-9-]+, instance [A-Za-z0-9._-]+ not starting with '.'"},"meta":{"repo":"…/acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

Write the entry with an empty default, `"${SWARMOPS_AGENT:-}"`, as the README does: the warning
goes, and an agent
started without an identity gets the clearer `E_IDENTITY_REQUIRED` on a write instead. Then give
each agent session its own `SWARMOPS_AGENT`, such as `agent/worker-1`, before it starts `claude`.

### Every write fails with `E_IDENTITY_REQUIRED`

The server has no identity: no `--as` and an empty or unset `SWARMOPS_AGENT`. Reads still work.
Give the session an identity, as above, and start the session again; a call cannot name one.

### Every call fails with `E_NOT_A_REPO`

The server opens the store for each call, so a wrong `--repo` shows on every call, not at start
(the text of a `repo_info` tool result):

```text
{"ok":false,"op":"repo_info","api":1,"error":{"code":"E_NOT_A_REPO","message":"…/nowhere is not a store: no FORMAT file","retryable":false,"exit":6,"details":{},"hint":"swarmops init …/nowhere"},"meta":{"repo":"…/nowhere","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

Without `--repo` and `SWARMOPS_REPO`, the message is `no store named: neither --repo nor
SWARMOPS_REPO gives one`. Fix the path in the registration. Give it absolute: the server resolves
a relative one against the directory the client started it in.

### A tool call fails with `E_INVALID_ARG`

The server adds rules to the request types, so a call the CLI would take can be refused here. Each
message says what to send instead:

- **`expected_old` is not a full id.** `ref_set` and `ref_delete` take the 64-hex id the ref held
  when the agent read it, never `main` or a prefix:
  `expected_old "main" is not a full changeset id, which a compare-and-swap needs: …`. The same rule
  holds for a `land`'s `resolutions.against`.
- **A relative `file` path.** `{"file": "a.json"}` is refused, since the server would read it
  from its own working directory: `the client file "a.json" is a relative path, which the server
  would read from its own working directory, not yours: …`. Give an absolute path, or send the
  bytes as `inline` or `base64`. A path that is the server's own stdin or stdout, or that is no
  regular file (a FIFO, a socket, a device, a directory), is refused too.
- **Arguments that do not fit the type**, such as a missing field:
  `` the arguments are not a RefSetReq: missing field `new` ``. The tool's `inputSchema` lists the
  fields; `swarmops schema RefSetReq` prints the type.

A message the server cannot serve as a request gets a JSON-RPC error instead of a tool result:
`-32700` parse error, `-32600` invalid request, `-32601` method not found, `-32602` invalid params,
`-32603` internal error (a bug), `-32022` unsupported protocol version. The module documentation
of `crates/swarmops-cli/src/mcp.rs` says when each is sent, and the README's
[MCP](../README.md#mcp) section gives the protocol versions the server speaks.

### The server exits during a session

The server exits 0 when the client closes its stdin and 1 when reading stdin or writing stdout
fails. It also ends, by `SIGABRT`, on any of the store's four documented aborts: a failed journal
write or sync whose cut-back fails too, frames that repair writes afresh landing anywhere but
where they were and not cut back, or an `init` that cannot take its `FORMAT` back (the README's
[Store, identity and lifetime](../README.md#store-identity-and-lifetime) lists them). The reason is
on the server's stderr. The client sees the server exit and has to start it again; start a new
session, run `swarmops verify`, and look at the filesystem's free space and health before you go
on.

## Where a store may live

A store must live on a local, native Linux filesystem: ext4 with its journal (what SwarmOps is
tested and measured on), XFS or btrfs. A store directory of mode `0700`, owned by the one user
every agent, server and command runs as, is the layout [operations.md](operations.md#before-you-start)
recommends:

```sh
install -d -m 0700 ~/.local/state/swarmops/repos
swarmops init ~/.local/state/swarmops/repos/acme --name acme
```

### `E_UNSUPPORTED_FS`

Exit 1, not retryable. The path leads under `/mnt/`, where WSL mounts Windows drives. It is
checked where the path leads, on `init` and on every open, so a symlink into `/mnt/` is refused
too:

```sh
swarmops init /mnt/d/acme
ln -s /mnt/d/stores/acme linked
swarmops --repo linked info
```

```text
{"ok":false,"op":"repo_init","api":1,"error":{"code":"E_UNSUPPORTED_FS","message":"store path /mnt/d/acme is not on a native Linux filesystem","retryable":false,"exit":1,"details":{"path":"/mnt/d/acme"},"hint":"put the store on a native Linux filesystem, such as under ~/.local/state/swarmops/repos/; /mnt/* holds Windows drives under WSL"},"meta":{"repo":"/mnt/d/acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
{"ok":false,"op":"repo_info","api":1,"error":{"code":"E_UNSUPPORTED_FS","message":"store path /mnt/d/stores/acme is not on a native Linux filesystem","retryable":false,"exit":1,"details":{"path":"/mnt/d/stores/acme"},"hint":"put the store on a native Linux filesystem, such as under ~/.local/state/swarmops/repos/; /mnt/* holds Windows drives under WSL"},"meta":{"repo":"linked","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

Put the store in the Linux filesystem, as the hint says. The check knows only `/mnt/`: NFS, SMB
and FUSE mounts, Docker Desktop's and virtiofs bind mounts among them, are neither detected nor
supported.

### `E_NOT_A_REPO`

Exit 6, not retryable. No store at that path, no store named, or a store of a layout this build
does not read:

```sh
swarmops --repo nowhere info
env -u SWARMOPS_REPO swarmops info
```

```text
{"ok":false,"op":"repo_info","api":1,"error":{"code":"E_NOT_A_REPO","message":"…/nowhere is not a store: no FORMAT file","retryable":false,"exit":6,"details":{},"hint":"swarmops init nowhere"},"meta":{"repo":"nowhere","elapsed_ms":0,"resolved":{},"auth":"trust"}}
{"ok":false,"op":"repo_info","api":1,"error":{"code":"E_NOT_A_REPO","message":"no store named: neither --repo nor SWARMOPS_REPO gives one","retryable":false,"exit":6,"details":{},"hint":"pass --repo <store> or set SWARMOPS_REPO; swarmops init <dir> creates a store"},"meta":{"elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

- No `FORMAT` file: a typo in the path, or a store not made yet. Check the path; `swarmops init`
  makes one.
- No store named: set `SWARMOPS_REPO`, or pass `--repo`. There is no default location.
- `FORMAT names a store layout this build does not read`: the store was made by another release.
  [operations.md](operations.md#upgrades) says what an upgrade across a `FORMAT` takes.

### `E_EXISTS`

Exit 1, not retryable. `init` found a store there already:

```text
{"ok":false,"op":"repo_init","api":1,"error":{"code":"E_EXISTS","message":"a store exists at acme already","retryable":false,"exit":1,"details":{},"hint":"swarmops --repo acme info"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

Use the store (the hint describes it), or pick another directory.

### Permission refused

The store's objects, refs and checkpoint are created readable by their owner only (mode `0600`).
A command that cannot read a file of the store fails with `E_IO`, `details.fs_op` and
`details.store_file` naming it. Here the checkpoint of a copy was made unreadable
(`chmod 000 chk/meta/journal.ckpt`):

```text
{"ok":false,"op":"repo_info","api":1,"error":{"code":"E_IO","message":"i/o error: read meta/journal.ckpt: Permission denied (os error 13)","retryable":false,"exit":1,"details":{"fs_op":"read","store_file":"meta/journal.ckpt"},"hint":null},"meta":{"repo":"chk","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

A command run as another Unix user meets the same refusal. Run every agent, server and command as
the user who owns the store, and check the mode of the file `store_file` names.

## Identity

Reads need no identity. A commit, a land, a ref move, an undo and a git import need one:
`--as <family>/<instance>`, else `SWARMOPS_AGENT`.

### `E_IDENTITY_REQUIRED`

Exit 5, not retryable.

```sh
swarmops commit -m c1 --changes c1.json
```

```text
{"ok":false,"op":"commit","api":1,"error":{"code":"E_IDENTITY_REQUIRED","message":"this operation changes the store, so it needs the caller's identity","retryable":false,"exit":5,"details":{},"hint":"pass --as <family>/<instance> or set SWARMOPS_AGENT, e.g. --as agent/worker-1"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

Pass `--as`, or export `SWARMOPS_AGENT`. Nothing was written. For an MCP server, see
[above](#every-write-fails-with-e_identity_required).

### An invalid agent name

An identity that is not `<family>/<instance>` is `E_INVALID_ARG` (exit 2), with
`details.source` naming where it came from:

```sh
SWARMOPS_AGENT=worker-1 swarmops commit -m c1 --changes c1.json
```

```text
{"ok":false,"op":"commit","api":1,"error":{"code":"E_INVALID_ARG","message":"SWARMOPS_AGENT: invalid agent name \"worker-1\": expected <family>/<instance> of at most 255 bytes","retryable":false,"exit":2,"details":{"source":"SWARMOPS_AGENT"},"hint":"agent names look like agent/worker-17: family [a-z0-9-]+, instance [A-Za-z0-9._-]+ not starting with '.'"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

The family is `[a-z0-9-]`, the instance `[A-Za-z0-9._-]` not starting with `.`: `agent/worker-1`,
`operator/ada`.

## Arguments and revisions

A revision is `main`, `release/<name>`, a changeset id or 12 or more of its first hex digits, then
any number of `~N` (the N-th first parent). The examples run on a store where `main` holds
`src/lib.rs`.

### `E_INVALID_ARG`

Exit 2, not retryable. A malformed argument, request or command line. The message names what is
wrong, and the hint often gives the grammar:

```sh
swarmops show 'not a rev'
swarmops log -n 0
```

```text
{"ok":false,"op":"show","api":1,"error":{"code":"E_INVALID_ARG","message":"\"not a rev\" is not a revision","retryable":false,"exit":2,"details":{"rev":"not a rev"},"hint":"a revision is main, release/<name>, a changeset id or 12 or more of its first hex digits, then optionally ~N for the N-th first parent"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
{"ok":false,"op":"log","api":1,"error":{"code":"E_INVALID_ARG","message":"limit must be at least 1","retryable":false,"exit":2,"details":{"limit":0},"hint":null},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

A usage error of the command line is this code too, with the usage line as its hint. Other causes
met in practice: a change list that is not a JSON array of changes (the hint gives an example, and
`swarmops schema ChangeInput` the rest), a change that names by id a blob the store does not hold
(the hint is `swarmops blob put <file>`), a `land --key` that landed another change already, a
`git export` to a branch checked out in a work tree ([Git export](#exporting-to-a-checked-out-branch)),
and the [MCP server's own rules](#a-tool-call-fails-with-e_invalid_arg).

### `E_REV_NOT_FOUND`

Exit 6, not retryable. The revision names no changeset: no such ref, an id the store does not
hold, or a `~N` past the first changeset.

```text
{"ok":false,"op":"show","api":1,"error":{"code":"E_REV_NOT_FOUND","message":"release/none: not found","retryable":false,"exit":6,"details":{"rev":"release/none"},"hint":"swarmops ref list"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

Run the hint: `swarmops ref list` for a ref name, `swarmops log` for an id or a `~N` walk.

### `E_AMBIGUOUS_ID`

Exit 1, not retryable. An id prefix of 12 or more hex digits matches more than one changeset. The
hint says to give more digits of the id; `swarmops log` shows whole ids. Scripts and agents should
pass the full 64-hex id, which never becomes ambiguous.

### `E_PATH_NOT_FOUND`

Exit 6, not retryable. Nothing is at that path at that revision.

```sh
swarmops cat main:src/none.rs
```

```text
{"ok":false,"op":"read_file","api":1,"error":{"code":"E_PATH_NOT_FOUND","message":"nothing is at \"src/none.rs\" in main","retryable":false,"exit":6,"details":{"path":"src/none.rs","rev":"main"},"hint":"swarmops ls main:src"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{"main":"82a6c2d1…"},"auth":"trust"}}
```

List the directory, as the hint does. A commit's `delete` of a path that holds no file fails the
same way.

### `E_IS_DIR`

Exit 1, not retryable. A file read named a directory:

```text
{"ok":false,"op":"read_file","api":1,"error":{"code":"E_IS_DIR","message":"\"src\" is a directory at main","retryable":false,"exit":1,"details":{"path":"src","rev":"main"},"hint":"swarmops ls main:src"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{"main":"82a6c2d1…"},"auth":"trust"}}
```

Use `swarmops ls` for a directory and `swarmops cat` for a file.

### `E_REF_NAME_INVALID`

Exit 1, not retryable. No store format takes the name: refs are `main` and `release/<name>`, and
in a FORMAT 2 store also branches (`feature/x`), tags (`tags/<path>`) and remote-tracking refs.
A FORMAT 2 name in a FORMAT 1 store is `E_FORMAT_REQUIRED` instead.

```sh
swarmops ref set main/x main --create
```

```text
{"ok":false,"op":"ref_set","api":1,"error":{"code":"E_REF_NAME_INVALID","message":"invalid ref name \"main/x\"","retryable":false,"exit":1,"details":{"name":"main/x"},"hint":"a ref name is main or release/<name>, and in a FORMAT 2 store a branch (feature/x), tags/<path> or remote/<remote>/<path>: each part of letters, digits, '.', '_' and '-', not starting with '.'"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{"main":"82a6c2d1…"},"auth":"trust"}}
```

An agent's own line of work needs no ref: it commits on a parent and lands the head. Use
`release/<name>` for a ref of your own, such as `release/feature-x`, or in a FORMAT 2 store a
branch such as `feature/x`. `git export` gives this code
too for a branch name git does not take; its hint is `git check-ref-format --branch`.

### `E_OP_NOT_FOUND`

Exit 6, not retryable. `op undo` of an operation the store does not have:

```text
{"ok":false,"op":"undo","api":1,"error":{"code":"E_OP_NOT_FOUND","message":"there is no operation 99 (the last is 1)","retryable":false,"exit":6,"details":{"last":1,"op":99},"hint":"swarmops op log"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

Operations are numbered from 0; `swarmops op log` lists them, newest first.

## Commits

### `E_EMPTY_COMMIT`

Exit 1, not retryable. The change list changes nothing: it is empty, or every put writes what the
path holds already.

```text
{"ok":false,"op":"commit","api":1,"error":{"code":"E_EMPTY_COMMIT","message":"the change list changes nothing","retryable":false,"exit":1,"details":{},"hint":"pass --allow-empty (allow_empty over MCP) to record a changeset that changes nothing"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{"main":"82a6c2d1…"},"auth":"trust"}}
```

Check the change list against the parent (`swarmops cat <parent>:<path>`), or pass
`--allow-empty` when an empty changeset is what you want.

### `E_TYPE_CONFLICT`

Exit 1, not retryable. A change puts an entry below a file, or a file where a directory is.
Here `src/lib.rs` is a file, and the change puts `src/lib.rs/x`:

```text
{"ok":false,"op":"commit","api":1,"error":{"code":"E_TYPE_CONFLICT","message":"src/lib.rs/x: a file and a directory conflict there","retryable":false,"exit":1,"details":{"path":"src/lib.rs/x"},"hint":null},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{"main":"82a6c2d1…"},"auth":"trust"}}
```

Delete the file in the same change list first (`{"path": "src/lib.rs", "op": "delete"}`), or
choose another path.

### `E_TOO_LARGE`

Exit 1, not retryable. A put's content over 512 MiB, a commit's contents over 1 GiB together, or a
change list's text over 64 MiB. `details` names the change, its size and the limit:

```text
{"ok":false,"op":"commit","api":1,"error":{"code":"E_TOO_LARGE","message":"change 0 (\"huge.bin\"): the content is 629145600 bytes, more than the 536870912 a put may hold","retryable":false,"exit":1,"details":{"file":"…/huge.bin","index":0,"limit":536870912,"path":"huge.bin","scope":"put","size":629145600},"hint":null},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{"main":"82a6c2d1…"},"auth":"trust"}}
```

A file over 512 MiB cannot be stored. Send large content as `{"file": <path>}` rather than
`inline` or `base64`, which keeps the change list's text small. For a commit whose contents pass
1 GiB together, store some of them first with `swarmops blob put` and name them by id
(`{"blob": "<id>"}`): a blob named by id counts nothing against that total. A `git import` of a
blob longer than an object holds fails with this code too.

Its `details` take one of four shapes, which [Errors](reference/errors.md#e_too_large) lists:
`{size, limit, scope}` above (`scope` `put`, `commit` or `request`); `{scope, size, limit,
remote}` for a push past a remote's limit; `{store_limit, max, got}` for an object of the store
past 512 MiB; and from `git import`, `{commit, path, size, max}` for a blob and `{object, kind,
size, max}` for a commit, a tree or a tag.

### `E_UNKNOWN_PARENT`

Exit 6, not retryable. Rare: the commit's parent was in the store when the commit started, and
was gone when it recorded, because a failed journal sync took the frame that held it back. Nothing
was recorded. A parent the store never held is `E_REV_NOT_FOUND` instead, since the parent is
resolved first. Run `swarmops log`, then commit on a changeset it shows.

### `E_CAS_MISMATCH`

Exit 4, retryable after a re-read. A ref moves only by compare-and-swap, and another writer moved
it first. Agent 2 advanced `main` while agent 1 held the old head:

```sh
swarmops ref set main 03b168a3… --expect 82a6c2d1…
swarmops commit -m ours --changes ours.json --parent 82a6c2d1… --advance main
```

```text
{"ok":false,"op":"ref_set","api":1,"error":{"code":"E_CAS_MISMATCH","message":"ref main moved to a2c75f68…: commit your change on top of it (--parent a2c75f68…), then retry with --expect a2c75f68…; re-apply your edits to the content at a2c75f68… rather than re-sending your old files","retryable":true,"exit":4,"details":{"actual":"a2c75f68…","name":"main"},"hint":"swarmops show a2c75f68…"},"meta":{"repo":"acme","elapsed_ms":0,…}}
{"ok":false,"op":"commit","api":1,"error":{"code":"E_CAS_MISMATCH","message":"ref main moved to a2c75f68…, so this commit recorded nothing: commit your change on top of it (--parent a2c75f68… --advance main), re-applying your edits to the content at a2c75f68… rather than re-sending your old files","retryable":true,"exit":4,"details":{"actual":"a2c75f68…","committed":false,"name":"main"},"hint":"swarmops show a2c75f68…"},"meta":{"repo":"acme","elapsed_ms":3,…}}
```

`details.actual` is what the ref holds now (`null` when it does not exist). Do not re-send your
old whole files on top of it: that reverts the other writer's edits. Land your change instead, and
the land merges it three-way onto the new head (here it conflicts, as the next section shows):

```sh
swarmops land 03b168a3…
```

To publish without a land, read the files you changed at `actual`, apply your edits to them, and
commit with `--parent <actual> --advance main`. An `op undo` that finds a ref moved since the
operation fails with this code as well, and writes nothing: undo the later operations first,
newest first, or land a change that reverses the operation.

## Landing and undo

### `E_CONFLICT`

Exit 3, not retryable as it is. The land's change and the target both changed the same lines, and
nothing was recorded. Continuing the example above, the land of `03b168a3…` (`--pretty`):

```text
{
  "ok": false,
  "op": "land",
  "api": 1,
  "error": {
    "code": "E_CONFLICT",
    "message": "1 path(s) of the change conflict with the target's head a2c75f68…: nothing was recorded",
    "retryable": false,
    "exit": 3,
    "details": {
      "report": {
        "base": "82a6c2d1…",
        "files": [
          {
            "base_blob": "f5f5220a…",
            "hunks": [
              {
                "base": {
                  "first": 1,
                  "last": 1,
                  "text": "pub fn f() {}\n"
                },
                "id": 0,
                "ours": {
                  "first": 1,
                  "last": 1,
                  "text": "pub fn ours() {}\n"
                },
                "theirs": {
                  "first": 1,
                  "last": 1,
                  "text": "pub fn theirs() {}\n"
                }
              }
            ],
            "kind": "content",
            "ours_blob": "d0ddd9c1…",
            "path": "src/lib.rs",
            "theirs_agent": "agent/worker-2",
            "theirs_blob": "7504e69e…",
            "theirs_changeset": "a2c75f68…"
          }
        ],
        "kinds": [
          "content"
        ],
        "ours": "03b168a3…",
        "theirs": "a2c75f68…",
        "truncated": false
      }
    },
    "hint": "swarmops cat a2c75f68…:src/lib.rs, then swarmops land 03b168a3… --resolve <file> (resolutions over MCP) with each path's resolution and against = a2c75f68…"
  },
  …
}
```

The report names each conflicting file, its lines on each side and the agent that last changed it
on the target. Resolve without a checkout: write a resolutions file against the report's
`theirs`, then land again with `--resolve`. Each path takes `{"take": "ours"}`,
`{"take": "theirs"}`, `{"content": …}` or `{"delete": true}` (`swarmops land --help` has the
format, and [operations.md](operations.md#2-agents-commit-and-land) a full run):

```json
{"against": "<the report's theirs, all 64 hex digits>",
 "files": {"src/lib.rs": {"content": {"inline": "pub fn both() {}\n"}}}}
```

A path the target changed again since the report is `stale`: the land fails with `E_CONFLICT`
again, naming it in `stale`. Read the new `theirs` and resolve against it. A report whose hunk
texts pass `--max-conflict-bytes` (32768 by default) leaves the texts out and says `truncated`;
read the sides with `swarmops cat`.

### `E_NOTHING_TO_LAND`

Exit 1, not retryable. The land has nothing to add: its head is on the target (`on_target`), or the
target holds its change already (`no_changes`).

```sh
swarmops land main
```

```text
{"ok":false,"op":"land","api":1,"error":{"code":"E_NOTHING_TO_LAND","message":"nothing to land: the head is on the target","retryable":false,"exit":1,"details":{"reason":"on_target"},"hint":null},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{"main":"a2c75f68…"},"auth":"trust"}}
```

Usually the change landed already, for example by an earlier run of the same land. To make such a
retry report the first land instead, give each land a `--key` (a uuid v7, say): run again with the
same key, it replays (`status: replayed`).

### `E_CAS_RETRY_EXHAUSTED`

Exit 4, retryable. The target moved before each of the land's attempts could publish, and nothing
was recorded. A land merges again and retries up to `--max-attempts` times (8 by default). This
one was forced with `--max-attempts 1` while two other writers advanced `main` in a loop:

```text
{"ok":false,"op":"land","api":1,"error":{"code":"E_CAS_RETRY_EXHAUSTED","message":"the target moved before each of the land's 1 attempt(s) could publish (it names bcc33bb8…): nothing was recorded","retryable":true,"exit":4,"details":{"actual":"bcc33bb8…","attempts":1},"hint":"run the land again: nothing of this one was recorded"},"meta":{"repo":"race","elapsed_ms":40,…}}
```

Run the land again, as the hint says. If it keeps coming at the default of 8 attempts, too many
writers publish to one ref at once; see [A busy store](#a-busy-store).

### `E_NOTHING_TO_UNDO`

Exit 1, not retryable. `op undo` of an operation that moved no ref, such as a plain commit without
`--advance`:

```text
{"ok":false,"op":"undo","api":1,"error":{"code":"E_NOTHING_TO_UNDO","message":"operation 0 moved no ref, so there is nothing to undo","retryable":false,"exit":1,"details":{"op":0},"hint":"swarmops op log"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

Find the operation that moved the ref with `swarmops op log` (its `ref_moves`) and undo that one.

## A busy store

### `E_BUSY`

Exit 4, retryable. Every journal, index and ref write holds the store's lock, `meta/lock` (an
`flock`). A writer waits up to 10 s for it, then gives up; nothing was committed. Here
`flock acme/meta/lock sleep 14`, standing in for a backup, held the lock for longer than that:

```sh
swarmops commit -m notes --changes notes.json --advance main
```

```text
{"ok":false,"op":"commit","api":1,"error":{"code":"E_BUSY","message":"the metadata store is busy: another writer holds meta/lock","retryable":true,"exit":4,"details":{},"hint":"retry: another writer held the store's lock for the whole wait"},"meta":{"repo":"acme","elapsed_ms":10009,"resolved":{"main":"a2c75f68…"},"auth":"trust"}}
```

Reads take no lock: a `swarmops log` run while the lock was still held answered at once. A read
can still get `E_BUSY` in one case: when commits land during every one of its 32 attempts to
take a snapshot. Its hint says which kind of wait ran out.

What to run:

1. Run the same command again. Nothing of the failed one was recorded (a commit's objects may
   remain, unnamed, and the retry reuses them).
2. If it repeats, find what holds the lock. In `lsof`'s `FD` column, `W` marks the holder (here
   a second hold of the same kind, `flock acme/meta/lock sleep 5`):

```sh
lsof acme/meta/lock
```

```text
COMMAND    PID USER   FD   TYPE DEVICE SIZE/OFF     NODE NAME
flock   816551  evo    3rW  REG    9,0        0 63116628 acme/meta/lock
sleep   816581  evo    3r   REG    9,0        0 63116628 acme/meta/lock
```

3. A backup under `flock` is the usual holder: hold the lock briefly, or copy the objects first
   without it ([operations.md](operations.md#backup-and-restore)). The kernel releases the lock
   when a process dies, so a crash never leaves a stale lock behind.

Under load alone, `E_BUSY` is rare. In the v0.1.0 measurement on the maintainers' host, 32
processes made 32,000 commits through the binary on one store, at 71.7 commits/s: none got
`E_BUSY`, and the slowest commit took 712 ms ([perf.md](perf.md)). A rising count means more
contention than the lock serves: too many writers on one store, a slow disk, or a long lock
holder. Watch it as [What to monitor](operations.md#what-to-monitor) shows. The same holds for
[`E_CAS_RETRY_EXHAUSTED`](#e_cas_retry_exhausted).

## A damaged store

A crash at any point leaves a store that the next writer repairs. Damage that no writer repairs,
such as a changed object file, is what `verify` reports. There is no repair tool yet (M6): the
remedy is a restore from a backup.

### What repairs itself at the next write

A writer that dies mid-write can leave a torn tail on the journal: the bytes of a transaction that
never reached its commit point. That is no damage. Reads pass over it, `verify` reports nothing,
and the next write cuts it off and says so in `meta.recovered`. Four stray bytes appended to a
copy's journal, then a commit:

```sh
printf 'torn' >> torn/meta/journal.log
swarmops --repo torn verify
swarmops --repo torn commit -m notes --changes notes.json --advance main --pretty
```

```text
{"ok":true,"op":"verify","api":1,"result":{"objects_checked":12,"changesets_checked":3,"problems":[],"findings":[],"truncated":false,"found":0},"meta":{"repo":"torn","elapsed_ms":1,"resolved":{},"auth":"trust"}}
{
  "ok": true,
  "op": "commit",
  …
  "meta": {
    "repo": "torn",
    "elapsed_ms": 50,
    "resolved": {
      "main": "a2c75f68…"
    },
    "auth": "trust",
    "recovered": {
      "log_cut": {
        "at": 484,
        "bytes": 4
      },
      "frames_listed": 0,
      "frames_rewritten": 0,
      "off_bytes_dropped": 0,
      "index_records_dropped": 0,
      "views_rebuilt": false,
      "records_replayed": 0,
      "refs_replayed": 0
    }
  }
}
```

The same repair lists frames a crashed writer committed but never listed, drops the torn end of
the index and of `journal.off`, finishes an interrupted index compaction, and replays changesets
and ref files from the journal; each count in `meta.recovered` says how much of which. A commit,
`ref set` and `ref delete` report it. It is no warning and needs no action. Seen now and then, it
means agents get killed; seen often, something keeps killing writers.

Two kinds of leftover are not repaired, and are safe to remove by hand with every writer stopped:
the files of interrupted writes in `objects/tmp/` and `meta/derived/tmp/`, and the derived values
in `meta/derived/` ([operations.md](operations.md#making-room)).

### `E_STORE_CORRUPT`

Exit 1, not retryable. Stored data failed verification. One byte of one object file changed, in a
copy of the store:

```sh
swarmops --repo flipped verify
swarmops --repo flipped verify --deep --pretty
```

```text
{"ok":false,"op":"verify","api":1,"error":{"code":"E_STORE_CORRUPT","message":"verify found a problem: object 03b168a3… is corrupt","retryable":false,"exit":1,"details":{"changesets_checked":3,"findings":[{"kind":"corrupt_object","object":"03b168a3…"}],"objects_checked":9,"problems":["object 03b168a3… is corrupt"]},"hint":"swarmops verify --deep"},"meta":{"repo":"flipped","elapsed_ms":2,"resolved":{},"auth":"trust"}}
{
  "ok": false,
  "op": "verify",
  "api": 1,
  "error": {
    "code": "E_STORE_CORRUPT",
    "message": "verify found a problem: object 03b168a3… is corrupt",
    "retryable": false,
    "exit": 1,
    "details": {
      "changesets_checked": 3,
      "findings": [
        {
          "kind": "corrupt_object",
          "object": "03b168a3…"
        }
      ],
      "objects_checked": 15,
      "problems": [
        "object 03b168a3… is corrupt"
      ]
    },
    "hint": "details.findings names each damaged object, record or ref by kind and id: restore them from a backup of the store"
  },
  …
}
```

The damaged object here is a changeset, which plain `verify` reads; damage to a blob shows only
under `--deep`, which re-hashes every object file. `details.findings` names each damaged object,
record or ref by kind (`corrupt_object`, `missing_object`, …) and id. Any other command that
meets the damage fails with this code too, and a metadata file that does not parse also names
`details.fs_op` and `details.store_file`, such as `{"fs_op": "read", "store_file":
"meta/journal.ckpt"}`.

What to run: stop every writer, keep the damaged store aside, and restore it
([below](#restore-from-a-backup)). Run `verify --deep` after a crash or a power loss, on every
backup, before an upgrade and on a schedule. [known-limitations.md](known-limitations.md#verification-and-repair)
lists the kinds of damage no writer repairs.

### `E_IO`

Exit 1, not retryable. The filesystem refused an operation. `details.fs_op` names the operation,
and `details.store_file` the file, relative to the store's root:

```sh
swarmops --repo nooff info
```

```text
{"ok":false,"op":"repo_info","api":1,"error":{"code":"E_IO","message":"i/o error: open meta/journal.off: No such file or directory (os error 2)","retryable":false,"exit":1,"details":{"fs_op":"open","store_file":"meta/journal.off"},"hint":null},"meta":{"repo":"nooff","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

A file of the store is gone (here `meta/journal.off`, removed by hand): restore the store. When
a journal append was refused for want of permission or on a read-only filesystem,
`details.committed` is `false` and the hint says what to fix first. The journal was made
read-only in a copy:

```text
{
  "ok": false,
  "op": "commit",
  "api": 1,
  "error": {
    "code": "E_IO",
    "message": "the transaction did not commit: a journal append failed, and what it wrote was cut back off the journal: append_durable meta/journal.log: Permission denied (os error 13)",
    "retryable": false,
    "exit": 1,
    "details": {
      "committed": false,
      "fs_op": "append_durable",
      "store_file": "meta/journal.log"
    },
    "hint": "make the store's files writable for this user, then run the same command again: nothing of this one was committed"
  },
  …
}
```

After `chmod u+w` on the file, the same command committed. Check the owner and mode of the
store's files (one Unix user, [above](#permission-refused)), whether the filesystem went
read-only (`dmesg`, `mount`), and its free space (`df`).

### `E_IO_TRANSIENT`

Exit 4, retryable. A journal append failed for another reason, and the store cut what it wrote
back off the journal: `details.committed` is `false`, and `details.fs_op` and
`details.store_file` name the append. Nothing was committed, so run the same command again. If it
keeps failing, look at the disk as for `E_IO`. A failure after the commit point is no error: the
operation succeeds with a warning in `warnings`, and the next write finishes the work.

### Restore from a backup

[operations.md](operations.md#backup-and-restore) gives the whole procedure. In short, with every
writer stopped (the agents' sessions and their MCP servers, and any script):

```sh
mv "$SWARMOPS_REPO" "$SWARMOPS_REPO.damaged"
cp -a "$BACKUP" "$SWARMOPS_REPO"
swarmops verify --deep
swarmops git import "$GIT_SIDE" --ref review --into main
swarmops git export "$GIT_SIDE" --ref main --branch review
```

The import takes back what was exported after the backup. What was written after the backup and
never exported is lost. Without any backup, a new store made from the git side (`swarmops init`,
then `swarmops git import` of each branch) exports to the same commits.

### `E_INTERNAL`

Exit 1, not retryable. A bug in SwarmOps, such as a caught panic. Run `swarmops verify --deep` to
check the store, then report it as a GitHub issue through the bug report template, which asks
for the commands, their envelopes, the `verify --deep` envelope and the SwarmOps version.

## Git import

`swarmops git import PATH` reads a git repository on this host. The examples import from
`acme.git`, a bare clone of a repository with the trunk `stable`.

### `E_GIT_NOT_A_REPO`

Exit 6, not retryable. No git repository at that path:

```text
{"ok":false,"op":"git_import","api":1,"error":{"code":"E_GIT_NOT_A_REPO","message":"no git repository it can open there: \"nowhere.git\" does not appear to be a git repository: Could not retrieve metadata of \"nowhere.git\": No such file or directory (os error 2)","retryable":false,"exit":6,"details":{"git_path":"nowhere.git"},"hint":null},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

Give the path of a work tree or of a bare repository; a relative path is read from the working
directory. `git export` gives the same code, with the hint `git init --bare <path>`.

### `E_GIT_REF_NOT_FOUND`

Exit 6, not retryable. `--ref` names nothing in the repository, or an object that is no commit:

```text
{"ok":false,"op":"git_import","api":1,"error":{"code":"E_GIT_REF_NOT_FOUND","message":"no ref and no commit is named \"no-such-branch\"","retryable":false,"exit":6,"details":{"git_ref":"no-such-branch"},"hint":"git -C acme.git for-each-ref"},"meta":{"repo":"acme","elapsed_ms":1,"resolved":{},"auth":"trust"}}
```

`--ref` is `HEAD` by default. The hint lists the repository's refs.

### `E_GIT_UNSUPPORTED`

Exit 1, not retryable. A repository the import does not read as it is; `details.reason` says why:

| `reason` | Means | What to do |
| --- | --- | --- |
| `shallow` | A shallow clone, whose oldest commits lack their parents. | `git -C <path> fetch --unshallow` (the hint) |
| `object_format` | A SHA-256 repository. | Not supported. |
| `ref_storage` | reftable refs. | Not supported. |
| `special_file` | A FIFO, a socket or a device where a file of the git directory should be (`details.file`). | Replace it with the file. |
| `file_too_long` | A ref, `config` or alternates file over 16 MiB (`details.file`, `size`, `max`). | Check the file. |
| `parents` | A commit of more than 255 parents (`details.commit`). | Not supported. |
| `mode` | An entry whose mode is no kind of entry (`details.commit`). | Not supported. |

```text
{"ok":false,"op":"git_import","api":1,"error":{"code":"E_GIT_UNSUPPORTED","message":"the repository is not read as it is: it is a shallow clone, whose oldest commits lack their parents","retryable":false,"exit":1,"details":{"reason":"shallow"},"hint":"git -C shallow.git fetch --unshallow"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

`git export` refuses the same first five for the repository it writes.

### `E_GIT_READ`

Exit 1, not retryable. An object the import needs is missing (`missing`, as in a partial clone,
which lacks blobs), does not hash to its id (`corrupt`) or does not parse (`malformed`), or a read
failed (`io`). A `--filter=blob:none` clone, into a new store:

```text
{"ok":false,"op":"git_import","api":1,"error":{"code":"E_GIT_READ","message":"the repository lacks object 5626abf0…","retryable":false,"exit":1,"details":{"object":"5626abf0…","reason":"missing"},"hint":"git -C partial.git fsck"},"meta":{"repo":"fresh","elapsed_ms":1,"resolved":{},"auth":"trust"}}
```

Import from a full clone. For damage, `git fsck` (the hint) says what git thinks of it.

### `E_GIT_IMPORT_LOSSY`

Exit 1, not retryable. A commit holds content a changeset cannot keep as git has it. The import
stops before that commit, and the target ref does not move. `details` names the `commit`, the
`path` and the `class`:

```sh
swarmops git import withsub --into release/sub
swarmops git import withsub --into release/sub --lossy gitlinks
```

```text
{"ok":false,"op":"git_import","api":1,"error":{"code":"E_GIT_IMPORT_LOSSY","message":"commit 40ef66a7… holds gitlinks content at lib/sub: a gitlink, to commit 8a314e05…; allowing the class drops or normalizes it","retryable":false,"exit":1,"details":{"class":"gitlinks","commit":"40ef66a7…","path":"lib/sub"},"hint":"run it again with --lossy gitlinks to drop or normalize it and mark the changeset (git.lossy)"},"meta":{"repo":"acme","elapsed_ms":1,"resolved":{},"auth":"trust"}}
{"ok":true,"op":"git_import","api":1,"result":{"imported":1,"skipped":0,"head_changeset":"d2cac5ec…","git_head_sha":"40ef66a7…","target_ref":"release/sub","ref_moved":true,"lossy":[{"commit":"40ef66a7…","changeset":"d2cac5ec…","classes":["gitlinks"]}],"lossy_total":1},"meta":{"repo":"acme","elapsed_ms":176,"resolved":{},"auth":"trust"}}
```

The classes are `gitlinks` (a submodule's entry), `paths` (a name that is not UTF-8, or `.` or
`..`), `modes`, `tree_order`, `empty_trees` and `nonstandard` (a commit whose bytes its fields
cannot give back). Allow a class with `--lossy <class>` (repeatable, or `all`) only when losing it
is fine: the changeset records the loss in its `git.lossy` metadata, and a history imported with
a loss exports as new commits, not as the commits it came from.

### `E_GIT_IMPORT_DIVERGED`

Exit 3, not retryable as it is. The target's head is not the import of `--ref`'s commit or of an
ancestor of it, so nothing was written; there is no silent merge. It happens when the git history
was rewritten, or when a changeset was committed on the target that `git export` never wrote.
Here a native commit went onto `main` while git gained the commit `three`:

```sh
swarmops commit -m native --changes b.json --advance main
swarmops git import acme.git --ref stable
```

```text
{"ok":false,"op":"git_import","api":1,"error":{"code":"E_GIT_IMPORT_DIVERGED","message":"main has diverged from the git history: its head ed56eaaf… stands for git commit cc5b5431…, which the tip 4dd39525… does not reach","retryable":false,"exit":3,"details":{"base":"cc5b5431…","git_head":"4dd39525…","target_head":"ed56eaaf…","target_ref":"main"},"hint":"swarmops log main"},"meta":{"repo":"acme","elapsed_ms":1,"resolved":{},"auth":"trust"}}
```

Merge the two sides in git, where a person can review the merge, then import the result:

1. Export the store's side to a branch of its own: `swarmops git export acme.git --branch review`.
2. Merge the trunk into it with git, in a clone of the git side, and push it back.
3. Import the merged branch: an import continues a head that `git export` wrote.

```sh
git -C work merge -q --no-edit origin/stable && git -C work push -q origin review
git -C acme.git log --oneline --graph review
swarmops git import acme.git --ref review --into main
```

```text
*   e15933b Merge remote-tracking branch 'origin/stable' into review
|\  
| * 4dd3952 three
* | c1c374f native two
* | cc5b543 native
|/  
* 8a314e0 two
* 222d9ba one
{"ok":true,"op":"git_import","api":1,"result":{"imported":2,"skipped":2,"head_changeset":"7ac0ad8a…","git_head_sha":"e15933bc…","target_ref":"main","ref_moved":true,"old_head":"ca467904…","lossy":[],"lossy_total":0},"meta":{"repo":"acme","elapsed_ms":153,"resolved":{},"auth":"trust"}}
```

The [daily loop](operations.md#the-daily-loop) avoids this: it imports what git gained before
agents land, and exports to a review branch rather than to the trunk. When the git history was
rewritten instead (a force-push), `details.base` names the commit the store stands on. To keep
both, import the rewritten history into a new ref (`--into release/<name>`); to follow git, create
the store again from it.

## Git export

`swarmops git export PATH` writes into a git repository on this host. It needs no identity.

### `E_GIT_EXPORT_DIVERGED`

Exit 3, not retryable as it is. The branch holds commits that are no export of the ref's head or
of an ancestor of it, so the branch did not move. `details.reason` says why, and the hint what
to do:

| `reason` | Means | What to do |
| --- | --- | --- |
| `branch_ahead` | Commits made in git on top of the export. | Import them first: `swarmops git import PATH --ref <branch>`. |
| `both_moved` | Git and the store both moved past the last export. | Merge in git, as for [`E_GIT_IMPORT_DIVERGED`](#e_git_import_diverged). |
| `branch_moved` | The branch names a commit that is no export of this history. | Export to another branch, or `--force`. |
| `source_moved_back` | The ref is behind the export the branch holds, after an undo or a `ref set` backwards. | `swarmops op log`, then decide; `--force` moves the branch back. |
| `lossy_history` | The branch holds the original commits of a history imported lossily. | Export to another branch, or `--force`. |
| `moved_during_export` | Another writer moved the branch while this export wrote. | Run the export again; what it wrote stays. |

```text
{"ok":false,"op":"git_export","api":1,"error":{"code":"E_GIT_EXPORT_DIVERGED","message":"the branch stable has diverged from ed56eaaf… (both_moved): it names 4dd39525…","retryable":false,"exit":3,"details":{"branch":"stable","exported_changeset":"9f4cb058…","git_head":"4dd39525…","head_changeset":"ed56eaaf…","reason":"both_moved"},"hint":"both git and main moved past 9f4cb058…, and nothing is merged: git -C acme.git log refs/heads/stable shows the git side, and --force replaces it"},"meta":{"repo":"acme","elapsed_ms":1,"resolved":{"main":"ed56eaaf…"},"auth":"trust"}}
```

`--force` moves the branch all the same and replaces what git holds there; read the git side
first (the hint's `git log`). Exporting to a review branch rather than to the trunk keeps commits
made in git and exports apart.

### Exporting to a checked-out branch

`E_INVALID_ARG` (exit 2), with or without `--force`: an export does not move the branch a work
tree has checked out, as `git push` does not.

```text
{"ok":false,"op":"git_export","api":1,"error":{"code":"E_INVALID_ARG","message":"the branch stable is checked out in the repository's work tree, which an export does not move","retryable":false,"exit":2,"details":{"branch":"stable","reason":"checked_out"},"hint":"detach the work tree's HEAD (git -C src switch --detach), or export to another branch (--branch <name>)"},"meta":{"repo":"acme","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

Export into a bare repository (`git init --bare`), or to another branch.

### `E_GIT_WRITE`

Exit 1, not retryable as it is. The repository could not be written; `details.reason` says what
failed: `ref_locked` (the branch's lock file exists), `ref_name_conflict` (a ref stands where the
branch's name needs a directory, or the other way round) or `io` (`details.path` names the file).
The branch is as it was, and the objects written stay. A lock file left in place:

```sh
swarmops git export acme.git --branch review
rm acme.git/refs/heads/review.lock
swarmops git export acme.git --branch review
```

```text
{"ok":false,"op":"git_export","api":1,"error":{"code":"E_GIT_WRITE","message":"refs/heads/review.lock is there: another writer holds the branch, or one that died left it","retryable":false,"exit":1,"details":{"path":"refs/heads/review.lock","reason":"ref_locked"},"hint":"once no git process runs in the repository, remove refs/heads/review.lock from its git directory (git -C acme.git rev-parse --git-path refs/heads/review.lock names it), as git itself says, then run the export again"},"meta":{"repo":"acme","elapsed_ms":149,"resolved":{},"auth":"trust"}}
{"ok":true,"op":"git_export","api":1,"result":{"exported":0,"objects_written":0,"head_changeset":"ca467904…","head_sha":"c1c374f6…","branch":"review","old_sha":"cc5b5431…","branch_moved":true,"incremental":true,"rewritten":0},"meta":{"repo":"acme","elapsed_ms":32,"resolved":{"main":"ca467904…"},"auth":"trust"}}
```

Remove the lock file only once no git process runs in the repository, as the hint says. The run
after it wrote no object (`objects_written: 0`), since the failed run had written them, and moved
the branch. An export that dies while it moves the branch leaves such a file too.

## Workspaces

`swarmops ws` and the `ws_*` tools work on a directory on this host
([Build and test a change](operate/build-and-test.md)). A workspace command finds its workspace by
`--ws <name>`, `--ws-path <dir>`, or the directory it runs in; `.swarmws/` in that directory is the
workspace's own state, not to be edited.

### `E_WS_EXISTS`

`ws create` found a workspace of that name made with other arguments (`details.path` is where).
Pick another name, or `ws delete` the old one. The same arguments again are no error: the answer
is the workspace, with `created: false`.

### `E_WS_NOT_FOUND`

No workspace by that name in the store's registry, or no `.swarmws/workspace.json` at or above the
directory. `ws list` lists the store's workspaces; a workspace whose entry is there but whose
directory is gone shows as an orphan in `ws list --check`, and `ws delete <name> --force` removes
the entry.

### `E_WS_LOCKED`

Exit 4, retryable. Another mutation of the same workspace held its lock for the whole 5 s wait:
two processes in one workspace is a misuse, and this is how it shows. Run the command again once
the other finishes. Reads (`ws status`, `ws diff`) take no lock.

### `E_WS_DIRTY`

The workspace holds edits the operation would overwrite or lose, listed in `details.paths`: a
`ws rebase` or an update over edited files, a write through a symlink the agent made, or a
`ws delete` with edits or commits not landed. Commit them (`ws commit -m <message>`, or `-m` on
`ws land`), restore them (`ws discard <path>`), or, for `ws delete`, take `--rescue`, which keeps
them as a changeset, or `--force`.

### `E_PATH_NOT_EMPTY`

`ws create` writes only into a directory that is missing or empty. Give another `--path`.

### `E_UNRESOLVED_CONFLICTS`

The workspace is rebasing, and `details.paths` still conflict: `ws status` lists them under
`rebasing.conflicts`. Edit each, `ws resolve <path>` (or `--take ours|theirs`), then
`ws rebase --continue`; `ws rebase --abort` gives up the rebase.

### `E_MARKERS_PRESENT`

`ws resolve` found a line that starts with `<<<<<<< ours`, `||||||| base` or `>>>>>>>` still in a
file it was asked to mark resolved. Remove the markers, or give `--force` if the file really holds such a line.

### `E_WS_UNPORTABLE`

The tree holds paths this workspace's volume cannot hold, and nothing was written: `details.reason`
is `case_collision` (two paths that differ only in case, on a case-insensitive volume, both in
`details.paths`), `windows_name` (a name Windows refuses) or `reserved_name` (a top-level
`.swarmws`, the workspace's own directory). Rename the paths in the store with a commit, or create
the workspace with `--prefix` paths that leave them out.

### `E_CHECK_FAILED`

The check of `ws land --check` (`details.stage: check`) or a new workspace's bootstrap command
(`stage: bootstrap`) exited non-zero or ran past its time (`timed_out`). `stdout_tail` and
`stderr_tail` hold the last 8 KiB of each stream, and `details.log` the whole output, relative to
the workspace. After a failed check nothing landed, and the workspace stands on the rebased
changeset (`details.changeset`), where the failure reproduces: fix, `ws commit`, and land again.
A check that needs more than 900 s takes `--check-timeout`.

## Codes not raised yet

`E_REF_NOT_FOUND` (exit 6) and `E_MAINTENANCE_BUSY` (exit 4, retryable) are in the schema so
that a client can match them now, but no operation raises them: no operation looks a ref up by
name (a missing ref is `E_REV_NOT_FOUND`), and gc's maintenance lease comes later
([known-limitations.md](known-limitations.md#serving-requests)). `E_AUTH` and `E_POLICY_DENIED`
are raised by `swarmops serve` since M14
([Serve over ssh](operate/serve-over-ssh.md#refusals-a-client-sees)). A client that meets a code newer
than its own table reads `exit` and `retryable` from the error itself.
