# Install SwarmOps on Linux and macOS

After this page, `swarmops` and `so`, one program with two names, run on your Linux host or Mac,
and you can pin, update, uninstall or build them.

**You need:** SwarmOps 0.2.0 or newer for the installer, 0.8.0 for the packages; Linux x86_64 or
aarch64, glibc 2.34 or newer (`ldd --version`), or [a Mac](#on-a-mac); `curl` or `wget`
(`command -v curl wget`).

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
swarmops-install: Installing SwarmOps 0.11.0 (x86_64-unknown-linux-gnu) into /home/me/.local/bin.
swarmops-install: Downloading https://github.com/KeiaiLab/SwarmOps/releases/download/v0.11.0/swarmops-0.11.0-x86_64-unknown-linux-gnu.tar.gz
swarmops-install: swarmops-0.11.0-x86_64-unknown-linux-gnu.tar.gz matches SHA256SUMS.
swarmops 0.11.0
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
swarmops 0.11.0
```

Under fish the hint is `fish_add_path "<dir>"`, for `~/.config/fish/config.fish`. When the
directory is on `PATH` already, the report has no hint at all.

What the installer does, in order:

1. Refuses a host the release builds do not run on, before it downloads anything: neither Linux
   nor macOS; on Linux, not x86_64 or aarch64, musl, or glibc older than 2.34; on macOS, an Intel
   Mac.
2. Finds the latest release from the redirect of `releases/latest`, so no GitHub API call or rate
   limit is involved.
3. Downloads the archive and `SHA256SUMS` over HTTPS into a temporary directory, which it removes
   however it ends. On macOS it reads `SHA256SUMS` first, and stops there when the release lists
   no macOS archive.
4. Checks the archive against its line in `SHA256SUMS`. On a mismatch, or with no line, nothing is
   installed.
5. Writes its receipt, `.swarmops-receipt`, into the directory: two lines that say the installer
   put the binaries there, and whether they may update themselves (not with `--no-auto-update`,
   a named version, or root).
6. Copies each binary under a temporary name into the directory, then renames it into place: an
   interrupted run leaves the old binary, never half a new one.
7. Runs `swarmops --version` from the directory and prints the report.

It needs `curl` or `wget`, `tar`, and `sha256sum` or `shasum`; macOS comes with `curl`, `tar` and
`shasum`. It sends no telemetry and writes only the install directory and its temporary directory.
For a release after 0.11.0 the report has one line more, which says whether the binaries update
themselves ([How do I update?](#how-do-i-update)):

```text
Automatic update is on. To turn it off: so update --auto off
```
The script is POSIX sh, so `| sh` works as well as `| bash`, and macOS's own bash 3.2 runs it.
`bash install.sh --help` lists every option.

### On a Mac

**You need:** a Mac with Apple silicon (`uname -m` prints `arm64`), macOS 11 or newer, and
SwarmOps 0.12.0 or newer.

The same command installs the Apple silicon build, `aarch64-apple-darwin`. 0.12.0 is the first
release that holds that build: 0.11.0 and every release before it hold Linux archives only, and
the release workflow builds the macOS archive from 0.12.0 on
([RELEASE.md](../../RELEASE.md#what-the-release-workflow-does)). Asked for one of the older
releases, as by `--version 0.11.0`, the installer stops before it downloads an archive
(`/Users/me` stands for your home directory):

```text
swarmops-install: Installing SwarmOps 0.11.0 (aarch64-apple-darwin) into /Users/me/.local/bin.
swarmops-install: SwarmOps 0.11.0 has no macOS build (its SHA256SUMS lists no swarmops-0.11.0-aarch64-apple-darwin.tar.gz): nothing was installed. Releases up to 0.11.0 hold Linux builds only. The releases: https://github.com/KeiaiLab/SwarmOps/releases
```

Against a release that holds the build, a first run prints this. It is the installer's output on
macOS 27 for an archive the release workflow's steps packed from this repository, with `X.Y.Z` put
for its version and GitHub's address for the test server's:

```text
swarmops-install: Installing SwarmOps X.Y.Z (aarch64-apple-darwin) into /Users/me/.local/bin.
swarmops-install: Downloading https://github.com/KeiaiLab/SwarmOps/releases/download/vX.Y.Z/swarmops-X.Y.Z-aarch64-apple-darwin.tar.gz
swarmops-install: swarmops-X.Y.Z-aarch64-apple-darwin.tar.gz matches SHA256SUMS.
swarmops X.Y.Z
Installed swarmops and so (its short form) in /Users/me/.local/bin.
/Users/me/.local/bin is not on PATH. Add this line to your shell startup file (~/.zshrc, or ~/.bash_profile under bash):
  export PATH="/Users/me/.local/bin:$PATH"
Quick start: https://github.com/KeiaiLab/SwarmOps#quick-start
Website: https://swarmops.cc
```

What differs from Linux:

- **The directory** is the same, `~/.local/bin`, or `/usr/local/bin` as root. macOS does not put
  `~/.local/bin` on `PATH`, so the first run prints the hint.
- **The hint's files** are `~/.zshrc`, for zsh, the default shell, and `~/.bash_profile` for bash:
  Terminal starts login shells, and a login bash does not read `~/.bashrc`. The fish hint is the
  same as on Linux.
- **An Intel Mac is refused.** Nothing is built or tested for `x86_64-apple-darwin`. In a shell
  that Rosetta translates on an Apple silicon Mac, where `uname -m` prints `x86_64`, the installer
  takes the arm64 build.
- **No C library check.** The binaries link against macOS's own libraries only.
- **The signature.** The binaries carry the linker's ad-hoc signature, without which no Apple
  silicon Mac starts a program; they have no Developer ID signature and are not notarized.
  `curl`, the installer's downloader, sets no quarantine mark on a download, so the installed
  binaries start. A browser sets one, and Gatekeeper may then refuse a copy unpacked from that
  download.
- **No package.** The `.deb` and `.rpm` are Linux's.

`--version`, `--dir`, an update and `--uninstall` work as on Linux.

## How do I pin a version?

Options go after `bash -s --`. `--version` installs one release instead of the latest:

```sh
curl -fsSL https://get.swarmops.cc/install.sh | bash -s -- --version 0.11.0
```

A leading `v` is dropped, so `v0.11.0` works too. `SWARMOPS_VERSION=0.11.0` does the same when no
`--version` is given. A version that has no release fails at the download, and installs nothing.

A pin stays: an install with a named version has automatic update off (its receipt says
`auto_update = false`), and the report says so for a release that reads the receipt:

```text
Automatic update is off, because a version was named. To turn it on: so update --auto on
```

Nothing moves such an install but the installer, run again, or `so update`, run by you.

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

On macOS the first line is `mktemp`'s own there, such as
`mktemp: mkstemp failed on /usr/local/bin/.swarmops.hyTvcd: Permission denied`.

## How do I install on many hosts?

Pin the version, so every host runs the same release, and keep a reviewed copy of the script with
your provisioning instead of piping it from the network each time:

```sh
curl -fsSL -o install.sh https://get.swarmops.cc/install.sh
bash install.sh --help                     # read it, then commit install.sh with your provisioning
SWARMOPS_VERSION=0.11.0 sh install.sh      # on each host
```

The script is the repository's `scripts/install.sh`. get.swarmops.cc serves its copy
`web/public/install.sh`, and `just check` fails when the two differ. Each host still checks its
download against the release's `SHA256SUMS`.

The pin also keeps the hosts the same afterwards: a named version turns automatic update off on
each of them, so no host moves to a newer release on its own
([How do I update?](#how-do-i-update)). To install the latest release on every host and still
keep them from updating themselves, pass `--no-auto-update` instead of a version:

```sh
sh install.sh --no-auto-update             # the latest release; it stays until you replace it
```

When you build a container image, pin the version there too. A container is not updated
unasked in any case (`update --status` says `off_container` where `/.dockerenv` or
`/run/.containerenv` exists), since what it installed would be gone with the container.

## How do I install the .deb or .rpm package?

From v0.8.0 on, each release also carries a `.deb` and an `.rpm` for each Linux architecture; a
Mac uses the installer or the archive. They hold
the same `swarmops` and `so` as the release's archives, byte for byte, installed into `/usr/bin`,
with `LICENSE`, `THIRD-PARTY-LICENSES`, `README.md` and `CHANGELOG.md` in `/usr/share/doc/swarmops`.
Download the one for your host and `SHA256SUMS`, and check it:

```sh
v=0.11.0                                  # the release's version
u=https://github.com/KeiaiLab/SwarmOps/releases/download/v$v
curl -fsSL -O "$u/swarmops_${v}_amd64.deb" -O "$u/SHA256SUMS"   # arm64 on aarch64
sha256sum --check --ignore-missing SHA256SUMS
sudo apt install "./swarmops_${v}_amd64.deb"
so --version
```

```text
swarmops_0.11.0_amd64.deb: OK
…
swarmops 0.11.0
```

On RHEL 9, its rebuilds such as AlmaLinux and Rocky Linux, and Fedora, the package is
`swarmops-0.11.0-1.x86_64.rpm` (`aarch64` on aarch64), installed with
`sudo dnf install ./swarmops-0.11.0-1.x86_64.rpm`. Either package manager installs what the
package needs, glibc 2.34 or newer and libgcc, which every supported host has.

The packages come from no repository, so `apt upgrade` and `dnf upgrade` do not update them:
install the next release's package the same way, and it replaces the old one. A package's
binaries never update themselves and `so update` refuses there (`details.reason` `no_receipt`):
the package manager owns the files. They carry no
signature of their own: `SHA256SUMS` from the release page is what checks them. The release
workflow installs every package in a Debian 12 and an AlmaLinux 9 container, runs both binaries
and removes it again before it publishes anything
([RELEASE.md](../../RELEASE.md#what-the-release-workflow-does)).

## How do I update?

**You need:** a release after 0.11.0 for everything here but the installer run again: 0.11.0
and older have no `update` command and never update themselves.

How the binaries got there decides how they are updated:

| Installed by | Updates itself? | To update by hand |
| --- | --- | --- |
| The installer | Yes, unless turned off (below) | `so update`, or the installer again |
| The installer with `--version`, `SWARMOPS_VERSION` or `--no-auto-update`, or as root | No | `so update`, or the installer again |
| A `.deb` or `.rpm`, or a server image | Never | Install the next release's package |
| A release archive unpacked by hand | Never | Unpack the next one; or run the installer once with `--dir`, which leaves a receipt |
| A clone (`cargo install`, `cargo build`) | Never | Build the newer source |

### Automatically

An install the installer made looks for a newer release by itself. After a command that went
well, at most once in 24 hours, it starts a second process that asks GitHub for the latest
release and, when the maintainers have marked that release for the installed minor line,
downloads it, checks it against the release's `SHA256SUMS`, runs the new binaries once and puts
them in place of the old ones. The command that started it does not wait: what it prints, its
exit code and its timing are the same with or without it.

The next command you run at a terminal then prints one line on stderr, after its output, once:

```text
swarmops: updated from 0.12.0 to 0.13.0. Notes: https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.13.0. Running mcp servers keep 0.12.0 until restarted.
```

A release the maintainers did not mark for your line is not installed, and the line says that
instead, once; one that could not be installed says why. A script or an agent's harness, whose
stderr is no terminal, is never shown a line: `so update --status` gives it the same as
`pending`. [`update`](../reference/commands/update.md) has every rule.

What an automatic update does not do for you:

- **It does not read the changelog.** A release with breaking changes is installed like any
  other; only a release that changes what stores older builds can open, or that needs a step by
  hand, is left for you to install
  ([What automatic update crosses](../policies/versioning-and-stability.md#what-automatic-update-crosses)).
  The printed line links the release's notes.
- **It does not check your stores.** Run `so verify --deep` when you want that check
  ([below](#by-hand)).
- **It does not restart anything.** A process that runs keeps the program it started with: a
  `swarmops mcp` server answers with the old version until its session ends.
- **It never touches a store** and never runs `upgrade`: a store stays in its format until you
  upgrade it ([Install and upgrade](../operate/install-and-upgrade.md#upgrade)).
- **It does not run everywhere.** Nothing looks for a release after `mcp`, `serve`,
  `auth git-credential` or `ws hook`, after a command that failed, where `CI` is set, in a
  container, for root, or in a directory that belongs to another user. A host that uses
  SwarmOps only through its MCP server never updates itself.

What it sends: one request a day of use to
`https://github.com/KeiaiLab/SwarmOps/releases/latest`, by your `curl`, with no token; when a
release is newer, one more whose address names the minor line you run
(`…/auto-update-from-0.12`); and, to install, the installer's own downloads. GitHub sees your
address and those paths. Nothing goes to swarmops.cc or to any host of the project's.

### How do I turn automatic update off?

```sh
so update --auto off
so update --status
```

```text
{"ok":true,"op":"update","api":1,"result":{"current":"0.12.0","updated":false,"auto_update":"off","dir":"/home/me/.local/bin"},…}
```

`--auto off` writes `auto_update = false` into the receipt beside the binaries,
`.swarmops-receipt`, so it holds for every user and process of that install until
`so update --auto on`. The other ways:

| To turn it off | Do this |
| --- | --- |
| for one install, from now on | `so update --auto off` |
| at install time | the installer's `--no-auto-update`, or a named `--version` |
| for one shell, script or service | `SWARMOPS_AUTO_UPDATE=off` in its environment |
| in CI | nothing: a set `CI` variable turns it off |
| in a container | nothing: it is off there |

`so update --status` reaches no network and says which of these decided: `on`, or `off` (the
receipt), `off_env`, `off_ci`, `off_container`, `off_owner` (root, or another user's directory),
`off_no_receipt` (a package or an archive) or `off_source_build` (a clone).
`SWARMOPS_AUTO_UPDATE=on` turns it on again where `CI` or a container turned it off, and nowhere
else. None of them stops `so update` when you run it yourself.

### By hand

```sh
so update --check      # what the latest release is; installs nothing
so update              # install it, when it is newer
```

`so update` installs the latest release whether or not it is marked, and never an older one or a
pre-release. It works on an install the installer made; elsewhere it says how the binaries are
updated instead. The installer run again does the same on any install of its own, and names the
version it replaced:

```text
swarmops 0.11.0
Replaced swarmops 0.11.0.
Installed swarmops and so (its short form) in /home/me/.local/bin.
```

To go back to an older release, or to stay on one, run the installer with `--version`
([How do I pin a version?](#how-do-i-pin-a-version)).

Before you update by hand, read the new version's section of
[CHANGELOG.md](../../CHANGELOG.md), which is also the release's notes on the releases page.
After it, check each [store](../concepts.md#store):

```sh
so verify --deep
```

`verify --deep` re-hashes every object and checks the derived values against the objects; a clean
store's [envelope](../concepts.md#envelope) has an empty `problems` list. Running MCP (Model Context
Protocol) servers keep the binary they started with until their sessions end.

### What can a new release break?

SwarmOps is pre-1.0, and its [versioning policy](../policies/versioning-and-stability.md#before-10)
says what a version bump may break before 1.0:

> a minor bump (0.1 to 0.2) may break a public surface other than the hash contract, and a patch
> bump (0.1.0 to 0.1.1) breaks none.

Every breaking change is called out under its version in [CHANGELOG.md](../../CHANGELOG.md), with
what to do about it. An automatic update installs such a release too, unless the release changes
what stores older builds can open or needs a step by hand
([What automatic update crosses](../policies/versioning-and-stability.md#what-automatic-update-crosses)).

A store's layout is named by its `FORMAT` file, here that of a store made by 0.4.0 to 0.11.0:

```sh
cat "$SWARMOPS_REPO/FORMAT"
```

```text
swarmops-store 2
```

A new `FORMAT` comes with the release that needs it. A newer binary reads and writes a store of
an older format as before, and changes its format only when you run `swarmops upgrade`, which no
older release can read afterwards; no update of the program runs it. A binary refuses a store
whose `FORMAT` it does not know, as `E_NOT_A_REPO` (exit 6) or `E_STORE_TOO_NEW`
([Install and upgrade](../operate/install-and-upgrade.md#upgrade)). The hash contract never
changes: a changeset's id is the same under every `FORMAT`.

## How do I uninstall?

Run the installer with `--uninstall`, and the same `--dir` if you gave one:

```sh
curl -fsSL https://get.swarmops.cc/install.sh | bash -s -- --uninstall
```

```text
Removed swarmops so from /home/me/.local/bin. Stores are left as they are.
```

It removes `swarmops` and `so` only if they are SwarmOps' own, and leaves another program by either
name alone. The installer's receipt goes with them, and what an update left beside them. A build from source is removed with `cargo uninstall swarmops-cli`, and a package with
`sudo apt remove swarmops` or `sudo dnf remove swarmops`. SwarmOps runs no daemon and keeps
no configuration file but that receipt, so nothing else of it is installed. A login you made
with `so auth login` is in a file of yours outside the install directory; `so auth logout --gh`
removes the login ([GitHub as a remote](github-remote.md)).

Your stores stay. A store is one directory, and no command removes one: delete it yourself
(`rm -r <store>`) once nothing needs its history, which goes with it. To keep the history of a
[ref](../concepts.md#ref), export it to git first
([Send the work back to git](git-round-trip.md#export-back-to-git)).

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
| `swarmops-<version>-aarch64-apple-darwin.tar.gz` | From 0.12.0 on: the same for Apple silicon Macs. |
| `swarmops_<version>_amd64.deb`, `swarmops_<version>_arm64.deb` | From v0.8.0 on: the x86_64 and aarch64 binaries as Debian packages ([installing them](#how-do-i-install-the-deb-or-rpm-package)). |
| `swarmops-<version>-1.x86_64.rpm`, `swarmops-<version>-1.aarch64.rpm` | The same as RPM packages. |
| `SHA256SUMS` | The SHA-256 of each archive and package. |

```sh
v=0.11.0                       # the release's version
t=x86_64-unknown-linux-gnu     # or aarch64-unknown-linux-gnu
u=https://github.com/KeiaiLab/SwarmOps/releases/download/v$v
curl -fsSL -O "$u/swarmops-$v-$t.tar.gz" -O "$u/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf "swarmops-$v-$t.tar.gz"
install -D -t ~/.local/bin "swarmops-$v-$t/swarmops" "swarmops-$v-$t/so"
~/.local/bin/swarmops --version
```

```text
swarmops-0.11.0-x86_64-unknown-linux-gnu.tar.gz: OK
swarmops 0.11.0
```

`--ignore-missing` skips the archive you did not download. On a Mac the target is
`t=aarch64-apple-darwin`, the check is `shasum`'s, and macOS's `install` has neither `-D` nor
`-t`, so the lines after the download become:

```sh
shasum -a 256 --check --ignore-missing SHA256SUMS
tar -xzf "swarmops-$v-$t.tar.gz"
mkdir -p ~/.local/bin
install -m 0755 "swarmops-$v-$t/swarmops" "swarmops-$v-$t/so" ~/.local/bin/
~/.local/bin/swarmops --version
```

With `gh`, the download step is:

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

With Rust 1.100 or newer, the workspace's `rust-version`, one command builds a tag and installs both
names into `~/.cargo/bin`:

```sh
cargo install --git https://github.com/KeiaiLab/SwarmOps --tag v0.11.0 --locked swarmops-cli
```

Its last lines, the build's cut (`…`):

```text
  Installing swarmops-cli v0.11.0 (https://github.com/KeiaiLab/SwarmOps?tag=v0.11.0#a747f7e1)
…
  Installing /home/me/.cargo/bin/so
  Installing /home/me/.cargo/bin/swarmops
   Installed package `swarmops-cli v0.11.0 (https://github.com/KeiaiLab/SwarmOps?tag=v0.11.0#a747f7e1)` (executables `so`, `swarmops`)
```

The zstd and BLAKE3 crates compile C, so the host also needs a C compiler. Add `--bin swarmops` to
install the long name alone, if the name `so` is taken. On macOS, v0.11.0 and older tags do not
hold the macOS work: build from a clone, below, with the Xcode command line tools as the C compiler
(`xcode-select --install`).

From a clone, `rust-toolchain.toml` pins Rust 1.100.0, which rustup installs on the first build;
rustup may warn that the toolchain file overrides your default toolchain, which is the pin at work:

```sh
git clone https://github.com/KeiaiLab/SwarmOps
cd SwarmOps
cargo install --path crates/swarmops-cli --locked
```

A plain `cargo build --release` leaves both binaries in `target/release/` instead.

## What does SwarmOps run on?

Linux, and from 0.12.0 on macOS on Apple silicon; Windows next. The
releases hold builds for x86_64 and aarch64 Linux, and their binaries need glibc 2.34 or newer,
which Ubuntu 22.04, Debian 12 and RHEL 9 have. The aarch64 Linux build is cross-compiled and
smoke-tested under QEMU, not tested on aarch64 hardware.

The macOS build is for Apple silicon (arm64) and macOS 11 or newer, the version it is built for.
It is built and run on macOS 27 only, on the maintainers' Mac, where the workspace's test suite
passes; CI does not run on macOS yet, and no older macOS was tried. 0.12.0 is the first release
that holds the macOS build, and there is no build for an Intel Mac.

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
are not detected yet, and not supported. On macOS: an APFS or journaled HFS+ volume; any other is
refused with `E_UNSUPPORTED_FS`.

Under WSL2, keep the store on its Linux filesystem, the ext4 virtual disk. No location is the
default: every command finds the store by `--repo` or `SWARMOPS_REPO`, and
`~/.local/state/swarmops/repos/<name>` is only the convention these guides use.

A store's objects and refs are readable by their owner only, so every agent and command that uses a
store runs as the Unix user who owns it. Its directories and journal follow your umask: keep a store
meant for one user under a directory of mode `0700`, made before the first `swarmops init` under it.

```sh
install -d -m 0700 ~/.local/state/swarmops/repos
```

## When the install does not work

The installer's own lines start with `swarmops-install:`; a line from `curl` or `mktemp` may come
before one. A usage error exits with status 2, every other error with status 1. The checks before
the download install nothing, and neither does a failed download or checksum. In the messages
below, `$dir`, `$machine` and `$archive` stand for the values the installer prints.

### The command is not found

The install succeeded, but the shell cannot find the new command:

```text
bash: so: command not found
```

The install directory is not on the shell's `PATH` yet; the installer's report said so, in a line
that starts `$dir is not on PATH. Add this line to …` (the next case). Add the line it printed and
open a new shell; until then `~/.local/bin/so --version` runs it by its full path.

### The install directory is not on PATH

The report holds this line, followed by the line to add:

```text
$dir is not on PATH. Add this line to your shell startup file (~/.profile, ~/.bashrc or ~/.zshrc):
```

Add the `export PATH=…` line it prints to that file (under fish, the `fish_add_path` line to
`~/.config/fish/config.fish`), open a new shell, and run `so --version`. The installer edits no
shell startup file itself.

### No build for this platform

```text
swarmops-install: No release build runs on $machine, only on x86_64 and aarch64. To build from source: https://github.com/KeiaiLab/SwarmOps#install
```

The CPU is neither x86_64 nor aarch64; nothing was installed.
[Build from source](#how-do-i-build-from-source). On a host that is neither Linux nor macOS the
installer prints `SwarmOps runs on Linux and macOS; Windows is next.`, and no release build runs
there yet; on musl or a glibc older than 2.34 it names the C library, and building from source is
the way. On a Mac it prints `SwarmOps 0.11.0 has no macOS build (…)` for a release from before the
macOS build, which is 0.11.0 and every release before it ([On a Mac](#on-a-mac)), and
`No release build runs on an Intel Mac (x86_64): …` on an Intel Mac.

### The checksum does not match

```text
swarmops-install: $archive does not match its line in SHA256SUMS: nothing was installed.
```

The download is not the published archive, and nothing was installed. Run the installer again; if
it repeats, report it ([SECURITY.md](../../SECURITY.md)). The line
`SHA256SUMS of v$version has no line for $archive: nothing was installed.` means the release
lists no such archive.

### Every other message

| The installer prints | Why | What to do |
| --- | --- | --- |
| `SwarmOps runs on Linux and macOS; Windows is next.` then `This system is FreeBSD. …` | The host is neither Linux nor macOS. | No release build runs there yet. |
| `SwarmOps 0.11.0 has no macOS build (its SHA256SUMS lists no swarmops-0.11.0-aarch64-apple-darwin.tar.gz): nothing was installed. Releases up to 0.11.0 hold Linux builds only. …` | A Mac, and a release whose `SHA256SUMS` has no line for the macOS archive: one from before the macOS build. | Install 0.12.0 or newer: it is the first release with a macOS build. |
| `No release build runs on an Intel Mac (x86_64): the macOS build is for Apple silicon (arm64) only. …` | A Mac with an Intel CPU. | No release build runs there. |
| `No release build runs on riscv64, only on x86_64 and aarch64. To build from source: …` | The CPU is neither x86_64 nor aarch64. | [Build from source](#how-do-i-build-from-source). |
| `This system's C library is musl; the release binaries need glibc 2.34 or newer. …` | Alpine and other musl systems. | Build from source. |
| `This system has glibc 2.31; the release binaries need 2.34 or newer. …` | The glibc is too old. | Build from source, or use a newer distribution. |
| `On glibc 2.35, each start of swarmops and so prints one ld.so line on stderr, …` | A notice, not an error: glibc 2.34 to 2.38. | Nothing; the install goes on. |
| `Neither curl nor wget is installed: install one, then run this again.` | No downloader. | Install `curl` or `wget`. |
| `Cannot find the latest release: … led to '…'.` | The redirect of `releases/latest` named no release. | Retry, or pin `--version`. |
| `curl: (22) The requested URL returned error: 404` then `Cannot download …: is 9.9.9 a release? …` | No release has that version. | Pick one from the [releases page](https://github.com/KeiaiLab/SwarmOps/releases). |
| `'latest' is not a release version: X.Y.Z or X.Y.Z-pre, such as 0.1.0.` | `--version` takes a version number only. | Leave `--version` out for the latest. |
| `Neither sha256sum nor shasum is installed, so the download cannot be checked.` | No checksum tool. | Install `sha256sum` (coreutils) or `shasum`. |
| `swarmops-0.11.0-x86_64-unknown-linux-gnu.tar.gz does not match its line in SHA256SUMS: nothing was installed.` | The download is not the published archive. | Run it again; if it repeats, report it ([SECURITY.md](../../SECURITY.md)). |
| `Cannot write in /usr/local/bin.` | The directory is not writable by you. | Use `sudo`, or another `--dir`. |
| `/home/me/.local/bin holds no SwarmOps binary: nothing was removed.` | `--uninstall` found nothing to remove there. | Give the `--dir` you installed into. |
| `Unknown option '--frobnicate'; see --help.` or `--dir needs a value; see --help.` | A usage error (status 2). | `bash install.sh --help`. |

## Sources

This guide draws on these documents, on the installer and on the v0.11.0 release:

- [scripts/install.sh](../../scripts/install.sh): the installer, its options and its messages.
- [Operating SwarmOps](../operations.md#install): Install, Before you start and Upgrades.
- [Toolchain, MSRV and platforms](../policies/msrv-and-platforms.md): Rust 1.100.0 and the platforms.
- [Versioning and stability](../policies/versioning-and-stability.md): what a release may break.
- [Releasing SwarmOps](../../RELEASE.md): how a release is built, checked and published.
- [v0.11.0 on the releases page](https://github.com/KeiaiLab/SwarmOps/releases/tag/v0.11.0): its
  archives, packages and `SHA256SUMS`.

## Next

**Next:** [Quick start](quick-start.md): it makes a store, [lands](../concepts.md#land) work from
several agents and reads the operation log.

- [Concepts](../concepts.md): one definition for each noun these guides use.
- [Install and upgrade](../operate/install-and-upgrade.md): the same install for an operator who
  runs a host for a swarm.
