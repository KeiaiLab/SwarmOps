# Goals

SwarmOps is a version control system, written in Rust, for swarms of coding agents: tens of agent
processes on one host committing into one repository at once, every command answering in JSON,
and the work going back to git so that CI and deployments run as before. It is built for one host
first and then, offline-first, for several. This page states what SwarmOps sets out to do, how
each goal is measured, where its target is written down, what has been measured so far, and which
milestone delivers it. [docs/perf.md](perf.md) holds every measurement,
[docs/known-limitations.md](known-limitations.md) what is still open, and
[docs/roadmap.md](roadmap.md) the milestones, in detail.

## Who it is for

- **People who run agent swarms.** 10 to 50 Claude Code or LangGraph processes working in one
  repository on one host, where agents in separate git worktrees serialize nothing and leave
  the merging to whoever pushes last.
- **The agents themselves.** A program reads a stable error code, an exit code and whether a
  retry can help; it does not parse prose. It reaches the store through the CLI or an MCP server.
- **Teams that keep git.** The store imports a git history and exports the landed work back to
  git, so a repository's CI, review and deployment keep their source of truth until the team
  decides otherwise. Planned: the store fetches from and pushes to GitHub itself, and people
  keep stock git against a store ([docs/roadmap.md](roadmap.md)).

## Goals

"Stated in" names the public document that states the target. "Measured" cites a section of
[docs/perf.md](perf.md), or says that no measurement exists yet. The milestones are those of
[docs/roadmap.md](roadmap.md); the order of those from M5 on is settled once the pilot has run.

| # | Goal | Metric | Target | Stated in | Measured | Milestone |
| --- | --- | --- | --- | --- | --- | --- |
| G1 | Many writers, one store | Commits by concurrent agent processes; `E_BUSY` failures | 10 to 50 processes; no `E_BUSY` (retryable, after the 10 s lock timeout) and no lost commit | README (intro); perf.md, "32 agents committing through the binary" (the plan's "0 failures") | 32 × 1,000 commits: `E_BUSY` 0, 71.7 commits/s, `meta.elapsed_ms` p99 621 ms, spread 1.00× ("v0.1.0 — `just perf` in a quiet window"). 50 processes: not yet measured | M1 (done); a FIFO hand-off on the lock, M6 |
| G2 | Cheap commits | Commit latency, syncs excluded | ≤ 5 ms + 0.3 ms per changed file | perf.md, "M0 fsync latency" and "32 agents committing through the binary"; known-limitations, "Full fsync is slow" | Without syncs: not yet measured. With full fsync: an uncontended commit 19 ms p50; 256 new objects written and synced on 32 threads in 72.9 ms, against the target's 82 ms ("v0.1.0 — `just perf` in a quiet window"; the 82 ms in "M1 store level") | M2's journal-sync decision, after the pilot |
| G3 | Fast reads | Lookup and snapshot latency | < 1 ms | perf.md, "Index lookups" | Index lookups ≤ 6.13 µs; a snapshot and lookup ≤ 152.9 µs p99 at 100,000 changesets; with 256 refs, 1.55 ms p50 and 3.00 ms p99, over the target ("v0.1.0 — `just perf` in a quiet window") | M1 (done); the many-refs case is not scheduled yet |
| G4 | Landing under a swarm's load | Lands/s, p99 latency, retry rate, lost lands, at 32 and 50 landing processes | At 50 processes: ≥ 20 lands/s, p99 < 500 ms, retries < 5 %, nothing lost | known-limitations, "Landing retries under a burst"; README, "Roadmap" | Not yet measured. A probe on a loaded host (known-limitations, same entry) landed every change exactly once, with 22 %, 44 % and 44 % of lands retried at 8, 16 and 32 processes | M4's landing measurements and landing model, after the pilot |
| G5 | Durability | Acknowledged commits lost after a crash or a power loss | 0, with full fsync as the only mode | README, "Why SwarmOps"; CONTRIBUTING, "The rules" | Crash sweeps, injected I/O errors and modelled power-loss images pass through the real binary; a literal `SIGKILL` on a real device is not tested yet (known-limitations, "Durability"). The fsync cost that keeps the default: p99 5.41 ms per durable write ("M0 fsync latency") | M1, M1.5 (done); M6 |
| G6 | An agent contract | Stable `E_*` codes, exit codes and schema; MCP | Codes, exit codes, the envelope and the schema change only as the versioning policy allows; every code pinned by a golden envelope | docs/policies/versioning-and-stability.md; known-limitations, "Golden envelopes cover 27 of the 37 error codes" | 37 codes, 27 pinned by golden envelopes; JSON Schema for 91 types (architecture.md, "The contract"); MCP server since M1.5 | M1, M1.5 (done); the remaining ten codes are not scheduled yet |
| G7 | A faithful git round trip | Commits missing or rewritten after import, land and export; lossy classes | None missing, `rewritten` 0, `git fsck --strict` passes; each of the six lossy classes refused unless `--lossy` names it | README, "Bring a git repository in" and "Send the work back to git"; docs/pilot.md, "Success criteria" 1 and 4 | Fixtures pass with three git versions every week (CONTRIBUTING, "Continuous integration"); real repositories: not yet measured (known-limitations, "No test reads a real repository yet") | M3, M8 (done); the pilot |
| G8 | Portability | Platforms built, tested and released | macOS and Windows beside Linux | README, "Roadmap"; docs/policies/msrv-and-platforms.md | x86_64 Linux tested; aarch64 Linux smoke-tested under QEMU | M12 |
| G9 | Workspaces | An agent builds and tests a change on disk before it lands it | Sparse workspaces, with an adapter for Claude Code's worktree hooks | README, "Roadmap" | Not built | M5 |
| G10 | Integrity tools | Repair, cleanup and detection | fsck and repair, gc, unsupported-filesystem detection, bounded reads, the next store `FORMAT` | README, "Roadmap"; known-limitations, "Verification and repair" | Not built; `verify --deep` checked 128,306 objects in 1,758 ms ("v0.1.0 — `just perf` in a quiet window") | M6 |
| G11 | Review | Change requests and a landing gate | Blame, change requests, their classification, approvals and a landing gate | README, "Roadmap" and "SwarmOps and git" | Not built | M7 |
| G12 | Simulation and a benchmark gate | Regressions caught before they land | The store under a deterministic simulator and random kills; `just bench` fails on a regression over 20 % | README, "Roadmap"; perf.md (its first paragraph) | No gate yet (known-limitations, "No benchmark gate") | M9 |
| G13 | Several hosts, offline-first (topology 3: local SwarmOps, remote SwarmOps) | Sync between hosts without an always-on server | A full store on every host; `so clone so://…`, `fetch`, `push`, `sync` and `sync status` against one upstream, over ssh; every confirmed land upstream exactly once | README, "Roadmap"; docs/roadmap.md, "Topology 3" | Today through a git remote, one writing host at a time (M8) | M11 |
| G14 | Safety | `unsafe`, fuzzing, supply chain | No `unsafe` code; four fuzz targets; `cargo deny` over every lockfile; workflows pinned and checked | CONTRIBUTING, "The rules"; SECURITY.md | Holds in every `just check` and in the weekly scheduled run | Every milestone |
| G15 | Works together with GitHub | The three topologies' acceptance criteria (G16, G17, G13) | All pass in CI with git 2.39.5, 2.43.0 and 2.56.0, and topology 2's in an opt-in run against GitHub | docs/roadmap.md, "Goal" | Not yet measured: no topology is served natively; topology 2 runs through a hand-moved bare clone (docs/roadmap.md, "Where we are") | M13, M14, M11 |
| G16 | Topology 1: local git, remote SwarmOps | Stock git's clone, fetch, pull and push against `swarmops serve`; ids after a round trip; a no-op fetch's cost | Every listed git command passes over ssh and HTTP; the same id at every ref and `git fsck --strict` clean; a no-op fetch and a clone at most 2× git's own server | docs/roadmap.md, "Topology 1" | Not yet measured; not built | M14 |
| G17 | Topology 2: local SwarmOps, remote GitHub | `so clone`, `fetch`, `push` and `sync` against a git remote; commits rewritten; CI on GitHub; cost of one new commit | `rewritten` 0 over any number of round trips; GitHub's `push` workflow runs unchanged; one commit's fetch or push at most 1.5× as costly on 12,000 commits as on 1,000 | docs/roadmap.md, "Topology 2" | Not yet measured; not built | M13 |

## Non-goals

- **A general-purpose git replacement.** Parity with git is not a v1 goal: no user hooks,
  signing, submodules or LFS.
- **A cluster.** SwarmOps is one program with no daemon and no external database. It does not
  scale horizontally, and an always-on RPC server is a follow-up after v1, not a milestone.
  The planned `swarmops serve` (M14, M11) runs per connection, under ssh or as a CGI program, so
  it adds no daemon.
- **Writes without a lock.** Object writes take no lock; metadata writes take one, which orders
  every commit and ref move.
- **Speed bought with durability.** No fast mode exists unless a measurement in
  [docs/perf.md](perf.md) justifies it.
- **A benchmark against git worktrees.** It is not on the roadmap; the README's comparison with
  git is judged from the design and the code.

## Engineering principles

- **Rust, with no `unsafe`.** `unsafe` is forbidden in every crate of the workspace.
- **Full fsync by default.** A commit returns only once its journal frame is on disk.
- **Serde types at the store's edge.** Store-facing types carry no filesystem paths, so every
  operation can later run behind an RPC boundary.
- **A fixed hash contract.** Byte layouts and the hash recipe never change in place; a new layout
  gets a new kind or version header.
- **Every claim measured.** A performance statement cites [docs/perf.md](perf.md), with the host,
  its load and the date.
- **Built from its own specification.** New work follows the
  [clean-room policy](policies/clean-room.md): design material from third parties reaches the
  code only as functional requirements in SwarmOps' own words.

## How goals change

A goal's target changes only with a measurement recorded in [docs/perf.md](perf.md) that
justifies the change, and a line in [CHANGELOG.md](../CHANGELOG.md) that says what changed and
why. A goal is added or dropped the same way, with the reason in the changelog. A measured value
in the table is replaced when a newer run in [docs/perf.md](perf.md) supersedes it.
