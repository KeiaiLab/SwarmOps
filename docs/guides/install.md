# Install SwarmOps on Linux

SwarmOps is one program with two names, `swarmops` and `so`. One command installs both; this guide
also covers pinning, updates, uninstalling and building from source.

## How do I install SwarmOps?

Run the installer:

```sh
curl -fsSL https://get.swarmops.cc/install.sh | bash
```

It downloads the latest release's archive for your machine, checks it against the release's
`SHA256SUMS`, and puts `swarmops` and `so` in `~/.local/bin`, or in `/usr/local/bin` when run as
root. It needs no sudo for its default directory. A first run on an x86_64 host whose `PATH` lacks
`~/.local/bin` prints this (`/home/me` stands for your home directory):

```text
swarmops-install: Installing SwarmOps 0.1.0 (x86_64-unknown-linux-gnu) into /home/me/.local/bin.
swarmops-install: Downloading https://github.com/KeiaiLab/SwarmOps/releases/download/v0.1.0/swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz
swarmops-install: swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz matches SHA256SUMS.
swarmops 0.1.0
Installed swarmops and so (its short form) in /home/me/.local/bin.
/home/me/.local/bin is not on PATH. Add this line to your shell startup file (~/.profile, ~/.bashrc or ~/.zshrc):
  export PATH="/home/me/.local/bin:$PATH"
Quick start: https://github.com/KeiaiLab/SwarmOps#quick-start
Website: https://swarmops.cc
```

The `swarmops-install:` lines go to stderr, the report to stdout. The installer edits no shell
startup file: if it prints the `PATH` line, add that line to the file it names, open a new shell,
and check:

```sh
so --version
```

```text
swarmops 0.1.0
```

Under fish the hint is `fish_add_path "<dir>"`, for `~/.config/fish/config.fish`. When the
directory is on `PATH` already, the report has no hint at all.

What the installer does, in order:

1. Refuses a host the release builds do not run on, before it downloads anything: not Linux, not
   x86_64 or aarch64, musl, or glibc older than 2.34.
2. Finds the latest release from the redirect of `releases/latest`, so no GitHub API call or rate
   limit is involved.
3. Downloads the archive and `SHA256SUMS` over HTTPS into a temporary directory, which it removes
   however it ends.
4. Checks the archive against its line in `SHA256SUMS`. On a mismatch, or with no line, nothing is
   installed.
5. Copies each binary under a temporary name into the directory, then renames it into place: an
   interrupted run leaves the old binary, never half a new one.
6. Runs `swarmops --version` from the directory and prints the report.

It needs `curl` or `wget`, `tar`, and `sha256sum` or `shasum`. It sends no telemetry and writes only
the install directory and its temporary directory. The script is POSIX sh, so `| sh` works as well
as `| bash`. `bash install.sh --help` lists every option.

## How do I pin a version?

Options go after `bash -s --`. `--version` installs one release instead of the latest:

```sh
curl -fsSL https://get.swarmops.cc/install.sh | bash -s -- --version 0.1.0
```

A leading `v` is dropped, so `v0.1.0` works too. `SWARMOPS_VERSION=0.1.0` does the same when no
`--version` is given. A version that has no release fails at the download, and installs nothing.

## How do I choose the directory?

`--dir` names the directory, and creates it if it is missing:

```sh
curl -fsSL https://get.swarmops.cc/install.sh | bash -s -- --dir /opt/swarmops/bin
```

`SWARMOPS_INSTALL_DIR` does the same when no `--dir` is given. Without either, the directory is
`/usr/local/bin` for root and `~/.local/bin` for everyone else. For a system-wide install, run the
installer as root:

```sh
curl -fsSL https://get.swarmops.cc/install.sh | sudo bash
```

A directory you cannot write fails before anything is replaced:

```text
mktemp: failed to create file via template '/usr/local/bin/.swarmops.XXXXXX': Permission denied
swarmops-install: Cannot write in /usr/local/bin.
```

## How do I install on many hosts?

Pin the version, so every host runs the same release, and keep a reviewed copy of the script with
your provisioning instead of piping it from the network each time:

```sh
curl -fsSL -o install.sh https://get.swarmops.cc/install.sh
bash install.sh --help                     # read it, then commit install.sh with your provisioning
SWARMOPS_VERSION=0.1.0 sh install.sh       # on each host
```

The script is the repository's `scripts/install.sh`. get.swarmops.cc serves its copy
`web/public/install.sh`, and `just check` fails when the two differ. Each host still checks its download
against the release's `SHA256SUMS`.

## How do I install the .deb or .rpm package?

From v0.8.0 on, each release also carries a `.deb` and an `.rpm` for each architecture. They hold
the same `swarmops` and `so` as the release's archives, byte for byte, installed into `/usr/bin`,
with `LICENSE`, `THIRD-PARTY-LICENSES`, `README.md` and `CHANGELOG.md` in `/usr/share/doc/swarmops`.
Download the one for your host and `SHA256SUMS`, and check it:

```sh
v=0.8.0                                   # the release's version
u=https://github.com/KeiaiLab/SwarmOps/releases/download/v$v
curl -fsSL -O "$u/swarmops_${v}_amd64.deb" -O "$u/SHA256SUMS"   # arm64 on aarch64
sha256sum --check --ignore-missing SHA256SUMS
sudo apt install "./swarmops_${v}_amd64.deb"
so --version
```

```text
swarmops_0.8.0_amd64.deb: OK
…
swarmops 0.8.0
```

On RHEL 9, its rebuilds such as AlmaLinux and Rocky Linux, and Fedora, the package is
`swarmops-0.8.0-1.x86_64.rpm` (`aarch64` on aarch64), installed with
`sudo dnf install ./swarmops-0.8.0-1.x86_64.rpm`. Either package manager installs what the
package needs, glibc 2.34 or newer and libgcc, which every supported host has.

The packages come from no repository, so `apt upgrade` and `dnf upgrade` do not update them:
install the next release's package the same way, and it replaces the old one. They carry no
signature of their own: `SHA256SUMS` from the release page is what checks them. The release
workflow installs every package in a Debian 12 and an AlmaLinux 9 container, runs both binaries
and removes it again before it publishes anything
([RELEASE.md](../../RELEASE.md#what-the-release-workflow-does)).

## How do I update?

Run the same command again. It replaces both binaries in place and names the version it replaced:

```text
swarmops 0.1.0
Replaced swarmops 0.1.0.
Installed swarmops and so (its short form) in /home/me/.local/bin.
```

Before you update, read the new version's section of [CHANGELOG.md](../../CHANGELOG.md), which is
also the release's notes on the releases page. After it, check each store:

```sh
so verify --deep
```

`verify --deep` re-hashes every object and checks the derived values against the objects; a clean
store's envelope has an empty `problems` list. Running MCP (Model Context Protocol) servers keep the
binary they started with until their sessions end.

### What can a new release break?

SwarmOps is pre-1.0, and its [versioning policy](../policies/versioning-and-stability.md#before-10)
says what a version bump may break before 1.0:

> a minor bump (0.1 to 0.2) may break a public surface other than the hash contract, and a patch
> bump (0.1.0 to 0.1.1) breaks none.

Every breaking change is called out under its version in [CHANGELOG.md](../../CHANGELOG.md), with
what to do about it. Every store today is `FORMAT 1`:

```sh
cat "$SWARMOPS_REPO/FORMAT"
```

```text
swarmops-store 1
```

A new `FORMAT` comes with the release that needs it. A binary refuses a store whose `FORMAT` it
does not know, as `E_NOT_A_REPO` (exit 6), and before 1.0 there is no migration tool: a store may
have to be created again, through git ([operations guide](../operations.md#upgrades)). The hash
contract never changes: a changeset's id is the same under every `FORMAT`.

## How do I uninstall?

Run the installer with `--uninstall`, and the same `--dir` if you gave one:

```sh
curl -fsSL https://get.swarmops.cc/install.sh | bash -s -- --uninstall
```

```text
Removed swarmops so from /home/me/.local/bin. Stores are left as they are.
```

It removes `swarmops` and `so` only if they are SwarmOps' own, and leaves another program by either
name alone. A build from source is removed with `cargo uninstall swarmops-cli`, and a package with
`sudo apt remove swarmops` or `sudo dnf remove swarmops`. SwarmOps keeps no
configuration file and runs no daemon, so nothing else of it is installed.

Your stores stay. A store is one directory, and no command removes one: delete it yourself
(`rm -r <store>`) once nothing needs its history, which goes with it. To keep the history of a ref,
export it to git first ([Send the work back to git](../../README.md#send-the-work-back-to-git)).

If you registered the server with Claude Code, remove it:

```sh
claude mcp remove swarmops
```

```text
Removed MCP server "swarmops" from local config
```

Also remove the `swarmops` entry from any project's `.mcp.json`, and drop `SWARMOPS_REPO` and
`SWARMOPS_AGENT` from your shell's startup files if you set them there.

## How do I check a release archive by hand?

Download the archive for your target and `SHA256SUMS`, check one against the other, and install the
two binaries yourself. Each release holds:

| Asset | What it holds |
| --- | --- |
| `swarmops-<version>-x86_64-unknown-linux-gnu.tar.gz` | The x86_64 build: `swarmops`, `so`, `LICENSE`, `THIRD-PARTY-LICENSES` (the licenses of the third-party crates compiled into the binaries), `README.md` and `CHANGELOG.md`, in a directory named after the archive. |
| `swarmops-<version>-aarch64-unknown-linux-gnu.tar.gz` | The same for aarch64. |
| `swarmops_<version>_amd64.deb`, `swarmops_<version>_arm64.deb` | From v0.8.0 on: the x86_64 and aarch64 binaries as Debian packages ([installing them](#how-do-i-install-the-deb-or-rpm-package)). |
| `swarmops-<version>-1.x86_64.rpm`, `swarmops-<version>-1.aarch64.rpm` | The same as RPM packages. |
| `SHA256SUMS` | The SHA-256 of each archive and package. |

```sh
v=0.1.0                        # the release's version
t=x86_64-unknown-linux-gnu     # or aarch64-unknown-linux-gnu
u=https://github.com/KeiaiLab/SwarmOps/releases/download/v$v
curl -fsSL -O "$u/swarmops-$v-$t.tar.gz" -O "$u/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf "swarmops-$v-$t.tar.gz"
install -D -t ~/.local/bin "swarmops-$v-$t/swarmops" "swarmops-$v-$t/so"
~/.local/bin/swarmops --version
```

```text
swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz: OK
swarmops 0.1.0
```

`--ignore-missing` skips the archive you did not download. With `gh`, the download step is:

```sh
gh release download "v$v" --repo KeiaiLab/SwarmOps \
  --pattern "swarmops-$v-$t.tar.gz" --pattern SHA256SUMS
```

v0.1.0's archives ship with SHA-256 checksums only, without a provenance attestation
([RELEASE.md](../../RELEASE.md#not-there-yet)).

The release workflow builds each archive from the tagged commit, smoke-runs its binaries, and checks
each archive against the SHA-256 its build wrote before it publishes anything
([What the release workflow does](../../RELEASE.md#what-the-release-workflow-does)).

## How do I build from source?

With Rust 1.99 or newer, the workspace's `rust-version`, one command builds a tag and installs both
names into `~/.cargo/bin`:

```sh
cargo install --git https://github.com/KeiaiLab/SwarmOps --tag v0.1.0 --locked swarmops-cli
```

Its last lines, the build's cut (`…`):

```text
  Installing swarmops-cli v0.1.0 (https://github.com/KeiaiLab/SwarmOps?tag=v0.1.0#12617736)
…
  Installing /home/me/.cargo/bin/so
  Installing /home/me/.cargo/bin/swarmops
   Installed package `swarmops-cli v0.1.0 (https://github.com/KeiaiLab/SwarmOps?tag=v0.1.0#12617736)` (executables `so`, `swarmops`)
```

The zstd and BLAKE3 crates compile C, so the host also needs a C compiler. Add `--bin swarmops` to
install the long name alone, if the name `so` is taken.

From a clone, `rust-toolchain.toml` pins Rust 1.99.0, which rustup installs on the first build;
rustup may warn that the toolchain file overrides your default toolchain, which is the pin at work:

```sh
git clone https://github.com/KeiaiLab/SwarmOps
cd SwarmOps
cargo install --path crates/swarmops-cli --locked
```

A plain `cargo build --release` leaves both binaries in `target/release/` instead.

## What does SwarmOps run on?

Linux today; macOS and Windows next. The releases hold builds for x86_64 and aarch64 Linux, and
their binaries need glibc 2.34 or newer, which Ubuntu 22.04, Debian 12 and RHEL 9 have. The aarch64
build is cross-compiled and smoke-tested under QEMU, not tested on aarch64 hardware.

`ldd --version` names a host's glibc version at the end of its first line:

```text
ldd (Ubuntu GLIBC 2.39-0ubuntu8.9) 2.39
```

On glibc 2.34 to 2.38, every start of the release `swarmops` and `so` binaries prints one `ld.so`
line on stderr, `weak version 'GLIBC_2.39' not found (required by …)`; exit codes, stdout envelopes
and the protocol `swarmops mcp` speaks are unaffected, and glibc 2.39 prints nothing. The installer
says so before it installs on such a host.

The [platform policy](../policies/msrv-and-platforms.md#linux-today-macos-and-windows-next) says
what the store needs from Linux.

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

## What do the installer's errors mean?

The installer's own lines start with `swarmops-install:`; a line from `curl` or `mktemp` may come
before one. A usage error exits with status 2, every other error with status 1. The checks before
the download install nothing, and neither does a failed download or checksum.

| The installer prints | Why | What to do |
| --- | --- | --- |
| `SwarmOps runs on Linux today; macOS and Windows are next.` then `This system is Darwin. …` | The host is not Linux. | No release build runs there yet. |
| `No release build runs on riscv64, only on x86_64 and aarch64. To build from source: …` | The CPU is neither x86_64 nor aarch64. | [Build from source](#how-do-i-build-from-source). |
| `This system's C library is musl; the release binaries need glibc 2.34 or newer. …` | Alpine and other musl systems. | Build from source. |
| `This system has glibc 2.31; the release binaries need 2.34 or newer. …` | The glibc is too old. | Build from source, or use a newer distribution. |
| `On glibc 2.35, each start of swarmops and so prints one ld.so line on stderr, …` | A notice, not an error: glibc 2.34 to 2.38. | Nothing; the install goes on. |
| `Neither curl nor wget is installed: install one, then run this again.` | No downloader. | Install `curl` or `wget`. |
| `Cannot find the latest release: … led to '…'.` | The redirect of `releases/latest` named no release. | Retry, or pin `--version`. |
| `curl: (22) The requested URL returned error: 404` then `Cannot download …: is 9.9.9 a release? …` | No release has that version. | Pick one from the [releases page](https://github.com/KeiaiLab/SwarmOps/releases). |
| `'latest' is not a release version: X.Y.Z or X.Y.Z-pre, such as 0.1.0.` | `--version` takes a version number only. | Leave `--version` out for the latest. |
| `Neither sha256sum nor shasum is installed, so the download cannot be checked.` | No checksum tool. | Install `sha256sum` (coreutils) or `shasum`. |
| `swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz does not match its line in SHA256SUMS: nothing was installed.` | The download is not the published archive. | Run it again; if it repeats, report it ([SECURITY.md](../../SECURITY.md)). |
| `Cannot write in /usr/local/bin.` | The directory is not writable by you. | Use `sudo`, or another `--dir`. |
| `/home/me/.local/bin holds no SwarmOps binary: nothing was removed.` | `--uninstall` found nothing to remove there. | Give the `--dir` you installed into. |
| `Unknown option '--frobnicate'; see --help.` or `--dir needs a value; see --help.` | A usage error (status 2). | `bash install.sh --help`. |

## What next?

The [quick start](quick-start.md) creates a store, lands work from several agents and undoes an
operation, and [Concepts](../concepts.md) defines the nouns these guides use.

## Sources

This guide draws on these documents, on the installer and on the v0.1.0 release:

- [scripts/install.sh](../../scripts/install.sh): the installer, its options and its messages.
- [README.md, Install](../../README.md#install): the archives, glibc and the store's filesystems.
- [Operating SwarmOps](../operations.md#install): Install, Before you start and Upgrades.
- [Toolchain, MSRV and platforms](../policies/msrv-and-platforms.md): Rust 1.99.0 and the platforms.
- [Versioning and stability](../policies/versioning-and-stability.md): what a release may break.
- [Releasing SwarmOps](../../RELEASE.md): how a release is built, checked and published.
- [v0.1.0 on the releases page](https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.1.0): its two
  archives and `SHA256SUMS`.
