# Install SwarmOps on Linux

SwarmOps is one program with two names, `swarmops` and `so`, installed from a release archive or
from source. This guide also covers upgrades and uninstalling.

## What does SwarmOps run on?

Linux today; macOS and Windows next. The release archives are for x86_64 and aarch64 Linux, and
their binaries need glibc 2.34 or newer, which Ubuntu 22.04, Debian 12 and RHEL 9 have. The aarch64
binaries are cross-compiled and smoke-tested under QEMU, not tested on aarch64 hardware.

`ldd --version` names a host's glibc version at the end of its first line. On glibc 2.34 to 2.38,
every start of the release `swarmops` and `so` binaries prints one `ld.so` line on stderr,
`weak version 'GLIBC_2.39' not found (required by …)`; exit codes, stdout envelopes and the
protocol `swarmops mcp` speaks are unaffected, and glibc 2.39 prints nothing.

The [platform policy](../policies/msrv-and-platforms.md#linux-today-macos-and-windows-next) says
what the store needs from Linux.

## How do I install a release?

Download the archive for your target and `SHA256SUMS` from the
[releases page](https://github.com/KeiaiLab/SwarmOps/releases), check the archive against
`SHA256SUMS`, unpack it, and copy `swarmops` and `so` into a directory on your `PATH`. The
repository is public: `curl` downloads without a login, and `gh` with your GitHub login.

A release carries one archive per Linux target, and `SHA256SUMS`:

| Asset | What it holds |
| --- | --- |
| `swarmops-<version>-x86_64-unknown-linux-gnu.tar.gz` | The x86_64 build: `swarmops`, `so`, `LICENSE`, `THIRD-PARTY-LICENSES` (the licenses of the third-party crates compiled into the binaries), `README.md` and `CHANGELOG.md`, in a directory named after the archive. |
| `swarmops-<version>-aarch64-unknown-linux-gnu.tar.gz` | The same for aarch64. |
| `SHA256SUMS` | The SHA-256 of each archive. |

Pick the version and the target:

```sh
v=0.1.0                        # the release's version
t=x86_64-unknown-linux-gnu     # or aarch64-unknown-linux-gnu
```

Download the archive and `SHA256SUMS` with `curl`:

```sh
u=https://github.com/KeiaiLab/SwarmOps/releases/download/v$v
curl -fL -O "$u/swarmops-$v-$t.tar.gz" -O "$u/SHA256SUMS"
```

or with `gh`:

```sh
gh release download "v$v" --repo KeiaiLab/SwarmOps \
  --pattern "swarmops-$v-$t.tar.gz" --pattern SHA256SUMS
```

Then check the archive, unpack it and install both names:

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf "swarmops-$v-$t.tar.gz"
install -D -t ~/.local/bin "swarmops-$v-$t/swarmops" "swarmops-$v-$t/so"
swarmops --version
```

The check and the version print:

```text
swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz: OK
swarmops 0.1.0
```

`install -D` creates `~/.local/bin` if it is missing. If that directory is not on your `PATH`, add
it, or install into a directory that is.

## How do I check a download?

`sha256sum --check --ignore-missing SHA256SUMS` checks each archive that `SHA256SUMS` lists and you
downloaded, prints `OK` for each match, and skips the archives you did not download. v0.1.0's
archives ship with SHA-256 checksums only, without a provenance attestation
([RELEASE.md](../../RELEASE.md#not-there-yet)).

The release workflow builds each archive from the tagged commit, smoke-runs its binaries, and checks
each archive against the SHA-256 its build wrote before it publishes anything
([What the release workflow does](../../RELEASE.md#what-the-release-workflow-does)).

## How do I build from source?

Clone the repository and run `cargo install`: it builds in release mode with Rust 1.99.0 and
installs both names into `~/.cargo/bin`. `rust-toolchain.toml` pins that Rust, and rustup installs
it on the first build. The zstd and BLAKE3 crates compile C, so the host also needs a C compiler.

```sh
git clone https://github.com/KeiaiLab/SwarmOps
cd SwarmOps
cargo install --path crates/swarmops-cli --locked
swarmops --version
```

rustup may warn that the toolchain file overrides your default toolchain: that is the pin at work.
Add `--bin swarmops` to install the long name alone, if the name `so` is taken. To install another
commit, check it out and run the same `cargo install`: it replaces both binaries. A plain
`cargo build --release` leaves both in `target/release/` instead.

## Where can a store live?

On a native Linux filesystem: ext4 with its journal, which SwarmOps is tested and measured on, XFS
or btrfs. A store under `/mnt/` (where WSL mounts Windows drives) is refused with
`E_UNSUPPORTED_FS`; NFS, SMB and FUSE mounts, Docker Desktop's and virtiofs bind mounts among them,
are not detected yet, and not supported.

Under WSL2, keep the store on its Linux filesystem, the ext4 virtual disk. No location is the
default: every command finds the store by `--repo` or `SWARMOPS_REPO`, and
`~/.local/state/swarmops/repos/<name>` is only the convention these guides use.

A store's objects and refs are readable by their owner only, so every agent and command that uses a
store runs as the Unix user who owns it. Its directories and journal follow your umask: keep a store
meant for one user under a directory of mode `0700`, made before the first `swarmops init` under it.

```sh
install -d -m 0700 ~/.local/state/swarmops/repos
```

## How do I upgrade?

Read the new version's section of [CHANGELOG.md](../../CHANGELOG.md), install the release over the
old one the same way, and check each store with `swarmops verify --deep`. Every store today is
`FORMAT 1`: a new binary with the same `FORMAT` is an install over the old one, and the changelog
says when a release brings a new one.

That section is also the release's notes on the releases page. Running MCP (Model Context Protocol)
servers keep the binary they started with until their sessions end.

```sh
cat "$SWARMOPS_REPO/FORMAT"
```

```text
swarmops-store 1
```

`verify --deep` re-hashes every object and checks the derived values against the objects; a clean
store's envelope has an empty `problems` list.

### What can a new release break?

SwarmOps is pre-1.0, and its [versioning policy](../policies/versioning-and-stability.md#before-10)
says what a version bump may break before 1.0:

> a minor bump (0.1 to 0.2) may break a public surface other than the hash contract, and a patch
> bump (0.1.0 to 0.1.1) breaks none.

Every breaking change is called out under its version in [CHANGELOG.md](../../CHANGELOG.md), with
what to do about it. A new `FORMAT` comes with the release that needs it. A binary refuses a store
whose `FORMAT` it does not know, as `E_NOT_A_REPO` (exit 6), and before 1.0 there is no migration
tool: a store may have to be created again, through git
([operations guide](../operations.md#upgrades)). The hash contract never changes: a changeset's id
is the same under every `FORMAT`.

## How do I uninstall?

Delete the two binaries from the directory you installed them into, or run
`cargo uninstall swarmops-cli` if `cargo install` put them in `~/.cargo/bin`. SwarmOps keeps no
configuration file and runs no daemon, so nothing else of it is installed; your stores stay until
you delete them.

```sh
rm ~/.local/bin/swarmops ~/.local/bin/so    # a release, installed as above
cargo uninstall swarmops-cli                 # or a build from source
```

A store is one directory, and no command removes one: delete it yourself (`rm -r <store>`) once
nothing needs its history, which goes with it. To keep the history of a ref, export it to git
first ([Send the work back to git](../../README.md#send-the-work-back-to-git)).

If you registered the server with Claude Code, remove it with `claude mcp remove swarmops`, and the
`swarmops` entry from any project's `.mcp.json`. Drop `SWARMOPS_REPO` and `SWARMOPS_AGENT` from
your shell's startup files if you set them there.

## What next?

The [quick start](quick-start.md) creates a store and publishes a first commit, and
[Concepts](../concepts.md) defines the nouns these guides use.

## Sources

This guide draws on these documents and on the v0.1.0 release:

- [README.md, Install](../../README.md#install): the archives, glibc and the store's filesystems.
- [Operating SwarmOps](../operations.md#install): Install, Before you start and Upgrades.
- [Toolchain, MSRV and platforms](../policies/msrv-and-platforms.md): Rust 1.99.0 and the platforms.
- [Versioning and stability](../policies/versioning-and-stability.md): what a release may break.
- [Releasing SwarmOps](../../RELEASE.md): how a release is built, checked and published.
- [v0.1.0 on the releases page](https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.1.0): its two
  archives and `SHA256SUMS`.
