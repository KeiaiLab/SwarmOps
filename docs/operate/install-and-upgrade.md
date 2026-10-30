# Install and upgrade

Put `swarmops` and `so` on a host, and move a host and its stores to a new release.
Commands and outputs here follow the
[conventions of the operations guide](../operations.md#how-to-read-these-pages).

## Before you start

- **Linux.** SwarmOps is built and tested on x86_64 Linux
  ([platforms](../policies/msrv-and-platforms.md)). The release builds run on x86_64 and aarch64
  Linux with glibc 2.34 or newer.
- **git** on the host, for `git clone` of this repository, and 2.39 or later for a store's git
  remote: `swarmops clone`, `fetch`, `push` and `sync` run it. `swarmops git import` and
  `swarmops git export` read and write git repositories themselves.
- For the installer: curl or wget, tar, and sha256sum or shasum.
- For a package: a Debian-family host with dpkg and apt, or a RHEL-family one with rpm and dnf.
- For a build from a clone: rustup and a C compiler.

Pick one of the four ways to install:

| Way | When |
| --- | --- |
| [The installer](#install-with-the-installer) | one command, the latest or a pinned release |
| [From a .deb or .rpm package](#install-from-a-deb-or-rpm-package) | the host's package manager owns the files (v0.8.0 on) |
| [From a clone](#install-from-a-clone) | a commit that is not a release |
| [From a release archive](#install-from-a-release-archive) | you download and check the archive yourself |

## Install with the installer

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

## Install from a .deb or .rpm package

From v0.8.0 on, each release carries one `.deb` and one `.rpm` per architecture, made from that
architecture's release archive: its `swarmops` and `so`, byte for byte, in `/usr/bin`, and its
`LICENSE`, `THIRD-PARTY-LICENSES`, `README.md` and `CHANGELOG.md` in `/usr/share/doc/swarmops`.
The `.deb` adds `/usr/share/doc/swarmops/copyright`, both license files in Debian's
machine-readable copyright format: an image that leaves out `/usr/share/doc` but keeps each
package's `copyright`, as `debian:12-slim` does, still holds the license texts.

| Host | Package |
| --- | --- |
| Debian 12, Ubuntu 22.04 or newer, x86_64 | `swarmops_<version>_amd64.deb` |
| The same, aarch64 | `swarmops_<version>_arm64.deb` |
| RHEL 9, its rebuilds (AlmaLinux, Rocky) or Fedora, x86_64 | `swarmops-<version>-1.x86_64.rpm` |
| The same, aarch64 | `swarmops-<version>-1.aarch64.rpm` |

1. Download the package and the checksums, and check the package:

   ```sh
   gh release download v0.8.0 --repo KeiaiLab/SwarmOps \
     --pattern 'swarmops_0.8.0_amd64.deb' --pattern SHA256SUMS
   sha256sum --check --ignore-missing SHA256SUMS
   ```

2. Install it with the host's package manager, which installs what it depends on (glibc 2.34 or
   newer, and libgcc), and check the version:

   ```sh
   sudo apt install ./swarmops_0.8.0_amd64.deb       # or: sudo dnf install ./swarmops-0.8.0-1.x86_64.rpm
   so --version
   ```

```text
swarmops_0.8.0_amd64.deb: OK
swarmops 0.8.0
```

The packages come from no apt or dnf repository, so `apt upgrade` and `dnf upgrade` do not see a
new release: install the next release's package the same way, which replaces the old one. They
carry no signature of their own; `SHA256SUMS` is what checks them. `sudo apt remove swarmops` or
`sudo dnf remove swarmops` removes the binaries and the documents, and no store. The release
workflow installs, runs, verifies and removes every package in a Debian 12 and an AlmaLinux 9
container before it publishes them ([RELEASE.md](../../RELEASE.md#what-the-release-workflow-does)).
A pre-release's packages carry its version as `0.8.0~rc.1`, which both package managers order
before `0.8.0`.

## Install from a clone

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

To have `--version` name the commit the binary was built from, set `SWARMOPS_BUILD_COMMIT` for
the build:

```sh
SWARMOPS_BUILD_COMMIT=$(git rev-parse HEAD) cargo install --locked --path crates/swarmops-cli
swarmops --version
```

```text
swarmops 0.7.0+69e435e36a6781b907a1fa1a5bd69caeff4c6db0
```

The commit follows a `+`, as semver build metadata, so the version itself and its order are
unchanged; compare it with `git rev-parse origin/stable` to see whether the installed binary is
current. The value must be 7 to 40 lowercase hex digits (an abbreviation such as
`git rev-parse --short HEAD` works too); any other value, the empty string included, fails the
build with a message that names the variable. Changing the variable rebuilds the binary.
Release archives and packages are built without it and print `swarmops <version>`.

## Install from a release archive

From v0.1.0 on, each release on the
[releases page](https://github.com/KeiaiLab/SwarmOps/releases) carries one archive per Linux
target, `swarmops-<version>-x86_64-unknown-linux-gnu.tar.gz` and
`swarmops-<version>-aarch64-unknown-linux-gnu.tar.gz` (`swarmops`, `so`, the license, the
third-party licenses, the README and the changelog), and a `SHA256SUMS` file to check them
against. The binaries need glibc 2.34 or newer. On glibc 2.34-2.38 hosts (Ubuntu 22.04, Debian 12,
RHEL 9) every start of the release `swarmops` and `so` binaries prints one `ld.so` line on stderr,
`weak version 'GLIBC_2.39' not found (required by …)`; exit codes, stdout envelopes and the MCP
protocol are unaffected, and on glibc 2.39 nothing is printed. [RELEASE.md](../../RELEASE.md) has
the release steps.

1. Download the archive and the checksums, check the archive, and unpack it:

   ```sh
   gh release download v0.1.0 --repo KeiaiLab/SwarmOps \
     --pattern 'swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz' --pattern SHA256SUMS
   sha256sum --check --ignore-missing SHA256SUMS
   tar -xzf swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz
   ```

2. Install both binaries and check the version:

   ```sh
   install -D -t ~/.local/bin swarmops-0.1.0-x86_64-unknown-linux-gnu/swarmops \
     swarmops-0.1.0-x86_64-unknown-linux-gnu/so
   ~/.local/bin/swarmops --version
   ```

```text
swarmops-0.1.0-x86_64-unknown-linux-gnu.tar.gz: OK
swarmops 0.1.0
```

## Upgrade

### Check the store's format

`init` makes a `FORMAT 2` store; one made by 0.3.0 or earlier, or by `init --format 1`, is
`FORMAT 1`:

```sh
cat "$SWARMOPS_REPO/FORMAT"
```

```text
swarmops-store 2
```

`swarmops info` reports the same number as `store_version`.

A binary opens only a store whose `FORMAT` it knows, and refuses any other as `E_NOT_A_REPO`
(exit 6). This is a store whose `FORMAT` file was edited to a layout this build does not know:

```json
{"ok":false,"op":"repo_info","api":1,"error":{"code":"E_NOT_A_REPO","message":"…/FORMAT names a store layout this build does not read","retryable":false,"exit":6,"details":{},"hint":null},…}
```

### Upgrade to a release with the same FORMAT

1. Install the new binary over the old one, the way you installed it
   ([installer](#install-with-the-installer), [package](#install-from-a-deb-or-rpm-package),
   [clone](#install-from-a-clone) or [archive](#install-from-a-release-archive)).
2. Running MCP servers keep the binary they started with until their sessions end: end the
   sessions to move them to the new one.
3. Check the store with `verify --deep` after the upgrade
   ([Verify and repair](verify-and-repair.md)).

### Upgrade to a release with a new FORMAT

A new `FORMAT` comes with the release that needs it, and [CHANGELOG.md](../../CHANGELOG.md)
says so. `FORMAT 2` (M6) adds a header with a sequence number and checksums to every journal
frame, a checksum to every `journal.off` entry, and ref names beyond `main` and
`release/<name>`. An older binary refuses the new store rather than misread it. A FORMAT 1 store
stays FORMAT 1 under the new binary, which reads and writes it as before, until it is upgraded:

1. Install the new binary, and end the sessions that run the old one, as for the same `FORMAT`.
2. Run `verify --deep` ([Verify and repair](verify-and-repair.md)), then upgrade:

   ```sh
   swarmops upgrade
   swarmops gc
   ```

   ```text
   {"ok":true,"op":"upgrade","api":1,"result":{"from":1,"to":2,"upgraded":true},…}
   {"ok":true,"op":"gc","api":1,"result":{"removed":[{"kind":"upgrade_residue","count":4}],"bytes":229,"orphan_objects":0},…}
   ```

`upgrade` cannot be undone: from then on no older release opens the store, and a process that
had it open as FORMAT 1, of an older release or of this one, fails its next read or write with
`E_STORE_CORRUPT`, and opens it again as FORMAT 2. An upgrade cut short is finished by the next
command that opens the store. `gc` then removes the FORMAT 1 files the upgrade left. On a FORMAT 2
store, `upgrade` does nothing (`"upgraded":false`). A `FORMAT` that `upgrade` does not reach
means creating the store again ([versioning policy](../policies/versioning-and-stability.md)).

### Create a store again

Creating a store again goes through git: export every ref you keep (`main` to the review
branch, each `release/<name>` to a branch of its own), make the new store with the new binary, and
import each branch into its ref. The new store exports to the same commits. What does not carry
over: changesets no ref reaches, the operation log (and so `undo`), and the changeset ids of the
changesets made in the store, which come back as imported changesets with ids of their own.

`GIT_SIDE` is the git side of the [daily loop](daily-loop.md#without-a-remote-a-bare-clone):

```sh
NEW=~/.local/state/swarmops/repos/acme-next
swarmops init "$NEW" --name acme
swarmops --repo "$NEW" git import "$GIT_SIDE" --ref review --into main
swarmops --repo "$NEW" git export "$GIT_SIDE" --ref main --branch review
```

```text
{"ok":true,"op":"repo_init","api":1,"result":{"repo_id":"393244461d7a5f01f7d600c8612a93f0","name":"acme","store_version":2,"changesets":0,"refs":0},…}
{"ok":true,"op":"git_import","api":1,"result":{"imported":10,"skipped":0,"head_changeset":"b9607b5d…","git_head_sha":"327bbc8898086c815a71e9ca1ab69bec147e5e36","target_ref":"main","ref_moved":true,"lossy":[],"lossy_total":0},…}
{"ok":true,"op":"git_export","api":1,"result":{"exported":0,"objects_written":0,…,"head_sha":"327bbc8898086c815a71e9ca1ab69bec147e5e36","branch":"review",…,"branch_moved":false,…,"rewritten":0},…}
```

The export writing nothing (`"exported":0`) is the check: the new store's history is the same
commits. The hash contract (the byte layouts of blobs, trees and changesets, and the hash recipe)
never changes: a changeset's id is the same under every `FORMAT`.
