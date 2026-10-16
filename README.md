# SwarmOps

[![CI](https://github.com/KeiaiLab/SwarmOps/actions/workflows/ci.yml/badge.svg?branch=stable)](https://github.com/KeiaiLab/SwarmOps/actions/workflows/ci.yml?query=branch%3Astable)

Source control for coding-agent swarms, in Rust: many agents commit to one store at once, every
command answers with one JSON envelope and a fixed exit code, and a crash at any point of a
write leaves a store that verifies clean. This is milestone M1: the native store, the repository
engine and the `swarmops` command line. The design is in `docs/superpowers/specs/`.

## Build

Rust 1.99 (`rust-toolchain.toml` pins it), on Linux:

```sh
cargo build --release     # target/release/swarmops
```

A store must live on a native Linux filesystem: a path under `/mnt/` (where WSL mounts Windows
drives) is refused with `E_UNSUPPORTED_FS`.

## Usage

```sh
export SWARMOPS_REPO=~/.local/state/swarmops/repos/acme   # or --repo <store> on each command
export SWARMOPS_AGENT=agent/worker-1                 # or --as <family>/<instance>

swarmops init "$SWARMOPS_REPO" --name acme
cat > changes.json <<'EOF'
[{"path": "src/lib.rs", "op": "put", "content": {"inline": "pub fn f() {}\n"}},
 {"path": "run.sh", "op": "put", "content": {"file": "./run.sh"}, "mode": "exec"}]
EOF
swarmops commit -m "first" --changes changes.json     # result.changeset: the new id
swarmops ref set main <id> --create                    # later: ref set main <new> --expect <old>
swarmops cat main:src/lib.rs
swarmops ls main:src
swarmops show main
swarmops log -n 20
swarmops verify --deep
```

| Command | What it does |
| --- | --- |
| `init <dir> [--name N]` | Create a store in `<dir>` (made if missing) and describe it. |
| `info` | Describe the store: its id, name, `main`, and how many changesets and refs it holds. |
| `whoami` | The identity this caller's changes carry. |
| `commit -m MSG --changes FILE\|- [--parent REV] [--allow-empty]` | Record a changeset from a JSON change list, on `--parent` or else on `main`'s head. It moves no ref. |
| `cat REV:PATH [--max-bytes N]` | One file at a revision: UTF-8 text, or base64. |
| `ls REV[:PATH] [--limit N] [--cursor C]` | One directory at a revision, a page at a time. |
| `show REV [--max-changes N]` | One changeset: parents, author, message, changes, signatures. |
| `log [REV] [-n N] [--cursor C]` | History along first parents, newest first (from `main` by default). |
| `ref list`, `ref set NAME REV (--expect OLD \| --create)`, `ref delete NAME --expect OLD` | List refs, or move one by compare-and-swap. |
| `verify [--deep]` | Check the store; `--deep` re-hashes every object. |
| `schema [TYPE]` | The JSON Schema of every request and response type, or of one. |

A revision is `main`, `release/<name>`, a changeset id or a unique prefix of 12 or more of its
hex digits, then any number of `~N` (the N-th first parent). A change list is a JSON array of
`{"path", "op": "put" | "delete", "content", "mode"}`: a put's `content` is `{"inline": "<UTF-8
text>"}`, `{"base64": "<bytes>"}` or `{"file": "<path>"}` (read by the CLI, on the client), at
most 512 MiB, and its `mode` is `file` (the default), `exec` or `symlink`; directories follow
from the paths. A ref moves only by compare-and-swap: when another agent moved it first,
`ref set` fails with `E_CAS_MISMATCH` (exit 4) and the ref's target in `details.actual`; commit
your change again on top of it (`--parent <actual>`) and retry with `--expect <actual>`.

### The store and the identity

`--repo <store>` names the store, else `SWARMOPS_REPO`. Reads need no identity. A commit or a
ref move needs one: `--as <family>/<instance>`, else `SWARMOPS_AGENT`, and `SWARMOPS_RUN_ID`
naming the agent's run if it has one. The changeset records it (and the journal records who
moved each ref); without one, a mutation fails with `E_IDENTITY_REQUIRED` (exit 5).

### Output and exit codes

Every command prints exactly one JSON envelope on stdout (only `--help` and `--version` print
plain text): pretty on a terminal, compact otherwise (`--pretty[=BOOL]` decides), or a short
text rendering with `--human` or `SWARMOPS_FORMAT=human`.

```json
{"ok":true,"op":"read_file","api":1,"result":{"blob":"f5f5…","size":14,"kind":"file","encoding":"utf8","content":"pub fn f() {}\n","truncated":false},"meta":{"repo":"/home/me/.local/state/swarmops/repos/acme","elapsed_ms":0,"resolved":{"main":"014c…"},"auth":"trust"}}
{"ok":false,"op":"commit","api":1,"error":{"code":"E_IDENTITY_REQUIRED","message":"this operation changes the store, so it needs the caller's identity","retryable":false,"exit":5,"details":{},"hint":"pass --as <family>/<instance> or set SWARMOPS_AGENT, e.g. --as agent/worker-1"},"meta":{"repo":"…","elapsed_ms":0,"resolved":{},"auth":"trust"}}
```

`op` names the operation (`cat` is `read_file`), and `meta.resolved` what each revision named.
A failure's `error` carries a stable `E_*` code, whether retrying the same command can succeed,
machine-readable `details`, and a `hint`, often the next command to run. The exit code is
`error.exit`:

| Exit | Meaning | For example |
| --- | --- | --- |
| 0 | Success. | |
| 1 | The operation failed. | `E_EMPTY_COMMIT`, `E_STORE_CORRUPT`, `E_IO` |
| 2 | A malformed request or command line. | `E_INVALID_ARG` |
| 4 | Retryable: the operation can succeed if run again (after a CAS mismatch, on top of `details.actual`). | `E_CAS_MISMATCH`, `E_BUSY`, `E_IO_TRANSIENT` |
| 5 | An identity or policy requirement refused it. | `E_IDENTITY_REQUIRED` |
| 6 | Something it names does not exist. | `E_NOT_A_REPO`, `E_REV_NOT_FOUND`, `E_PATH_NOT_FOUND` |

`swarmops schema` gives every type and error code; the operations spec has the full table.

### The store on disk

A store is one directory. `FORMAT` names its layout version (`init` writes it last, so a store
without it was never finished), and `REPO` the repository's id and name. `objects/` holds one
self-verifying, content-addressed file per blob, tree and changeset, in 256 fan-out directories
(`objects/<2 hex>/<62 hex>`), staged through `objects/tmp/`. `meta/` holds the metadata: an
append-only, CRC-framed journal (`journal.log`, with each frame's offset in `journal.off` and
how far it is materialized in `journal.ckpt`) that is its source of truth, the changeset index
built from it (`idx/`, `parents.log`, `sorted/`), one file per ref (`refs/`), and the lock that
serializes writers (`lock`). Every object is durable before any metadata names it, and a write
commits when its journal frame is durable, so the next write repairs whatever a crash cut short.

## Development

```sh
just check        # the definition of done: fmt, clippy -D warnings, tests, docs, cargo-deny
just check-slow   # more proptest cases, and the ignored tests: crash sweeps, contention, ...
just perf         # the M1 measurements, optimized; docs/perf.md records them
just fuzz [target]  # cargo-fuzz on nightly: codec_roundtrip, tree_decode, frame_decode, journal_recovery
just bench
```

`scripts/check.sh` runs `just check`'s steps on a machine without `just`. Tests never need the
network; those that read real repositories on this host run only with `SWARMOPS_HOST_REPOS=1`.

### The `test-hooks` feature

The crash tests drive three test-only environment variables: `SWARMOPS_TEST_CRASH_AT=<k>` aborts
the process at its k-th store filesystem call, `SWARMOPS_TEST_FS_CALLS=<file>` makes it write how
many calls it made, and `SWARMOPS_TEST_NOW=<unix seconds>` fixes the time it signs changesets at.
They act only in a build with the cargo feature `test-hooks` (on `swarmops-store`,
`swarmops-repo`, `swarmops-ops` and `swarmops-cli`; `swarmops-cli/test-hooks` turns on the
rest). The gates build the tests with `--all-features`; a plain `cargo test` skips the tests that
need the hooks. Every other build, `cargo build --release` included, ignores the variables, and
`just check` runs a release build to show it.

## Specs and docs

- `docs/superpowers/specs/2026-10-15-swarmops-design.md`: the design, which the specs below
  refine; specs are the authority.
- `docs/superpowers/specs/2026-10-15-swarmops-storage-core-design.md`: objects, the journal,
  refs, crash safety, and the canonical byte layouts and hash recipe (the hash contract).
- `docs/superpowers/specs/2026-10-15-swarmops-operations-landing-design.md`: the operations,
  the CLI, the envelope and the error codes, workspaces and landing.
- `docs/superpowers/specs/2026-10-15-swarmops-review-model-design.md`: change requests, their
  classification and approvals, and the landing gate (after M1).
- `docs/superpowers/plans/`: the milestone plans; `docs/perf.md`: the measurements.

## Project

- [CHANGELOG.md](CHANGELOG.md): what has changed; nothing is released yet.
- [CONTRIBUTING.md](CONTRIBUTING.md): how changes are made, the tests' rules, and how AI
  assistance is attributed. [AGENTS.md](AGENTS.md) holds the rules every coding agent here
  follows.
- [SECURITY.md](SECURITY.md): how to report a vulnerability privately, and what counts as one.
- [docs/architecture.md](docs/architecture.md): the design in English, as an overview of the
  Korean specs.
- [docs/known-limitations.md](docs/known-limitations.md): what M1 leaves open, and which
  milestone takes each item.
- [docs/policies/versioning-and-stability.md](docs/policies/versioning-and-stability.md): what
  stays compatible. [docs/policies/msrv-and-platforms.md](docs/policies/msrv-and-platforms.md):
  the Rust version and the platforms SwarmOps supports.
- [RELEASE.md](RELEASE.md): the release checklist.

## License

SwarmOps is released under the [MIT License](LICENSE).
