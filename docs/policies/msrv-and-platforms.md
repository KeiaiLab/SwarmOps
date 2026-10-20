# Toolchain, MSRV and platforms

## The toolchain

`rust-toolchain.toml` pins Rust **1.99.0** (with rustfmt, clippy and llvm-tools), so every
local build and every CI run uses exactly that compiler. The one exception is fuzzing:
cargo-fuzz needs a nightly toolchain, so `just fuzz` runs `cargo +nightly fuzz`, while
`just check` formats and lints the fuzz crate on the pinned toolchain.

## The MSRV is the pinned toolchain

`rust-version = "1.99"` in the workspace `Cargo.toml` makes the minimum supported Rust version
the pinned one, for now. Three reasons:

- SwarmOps ships two binaries, `swarmops` and `so` (the same program), and publishes no crate,
  so no one else compiles it as a dependency with an older compiler.
- One toolchain is the one that is tested. An MSRV below the pin would need a CI job of its own
  to mean anything, and CI runs on small runners with a minutes budget.
- The code uses recent standard-library features: the metadata lock is `std::fs::File::lock`
  and `try_lock` (stable since Rust 1.89), and the workspace uses edition 2024.

That holds until a crate is published or someone needs to build SwarmOps with an older compiler.
Then the MSRV gets a policy of its own (such as uv's "never newer than N−2") and a CI job that
builds at it. Moving to a newer Rust is a change of its own: `rust-toolchain.toml` and
`rust-version` move together, `just check` passes on the new compiler (new lints included), and
the changelog notes it.

## Linux only

SwarmOps supports Linux, and is built and tested on x86_64 Linux. Other systems (macOS, Windows,
the BSDs) are not tested and not supported.

The store relies on what a native Linux filesystem gives:

- **`flock`**: one exclusive lock on `meta/lock` serializes writers across processes, and the
  kernel releases it when the holder dies, so a crash leaves no stale lock. Waiters queue in the
  kernel.
- **`fsync` and `fdatasync` that mean it**, on files and on directories: a commit is durable
  when its journal frame's `fdatasync` returns, and a new file's name is durable once its
  directory is synced.
- **Atomic `rename`, and hard links within the filesystem**: objects are linked into place
  without replacing anything, and ref files and checkpoints are replaced by rename.
- **A failed directory `fsync` that stops the filesystem** (storage spec §2 rule 5): on ext4
  with its journal, XFS and btrfs, a directory fsync that fails once makes every later change
  and sync of that filesystem fail until it is mounted again, so a later fsync of a directory
  that succeeds makes every entry made in it before durable. A failed writeback of file data is
  not fail-stop (Linux reports it once, to one sync), so the store cuts back or writes again
  what a failed sync covered. ext4 without its journal, ext2, FUSE and network filesystems do
  not stop so, and are not supported.

So a store must live on a native Linux filesystem:

- A store whose path is under `/mnt/` (where WSL mounts Windows drives) is refused with
  `E_UNSUPPORTED_FS`. The check looks at the canonicalized path only.
- Network and FUSE filesystems (NFS, SMB, sshfs) are not detected yet (detection by `statfs` is
  planned for M6) and not supported: their locking and sync semantics differ.
- ext4 is what SwarmOps is tested and measured on. One crash-recovery step, resuming an
  interrupted `swarmops init`, relies on ext4's and XFS's single ordered journal; on btrfs a
  resumed `init` could leave the crashed attempt's directories not durable (recoverable by
  creating the store again).

WSL2 is supported for stores on its Linux filesystem (the ext4 virtual disk), not under `/mnt/`.
Some tests read Linux's `/proc/locks`.

## Where the numbers come from

Every measurement in [docs/perf.md](../perf.md) was taken on one host: WSL2 (kernel
5.15.153.1), ext4 (`data=ordered`) on the virtual disk, an AMD Ryzen 9 7950X3D (16 cores, 32
threads) and 106 GiB of RAM, where one `fsync` takes about 4 ms. Other disks will give other
numbers. CI runs on GitHub's hosted `ubuntu-24.04` runners: they check correctness, and nothing
records or compares their timings.
