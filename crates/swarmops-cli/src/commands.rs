//! Running a command (operations spec §1): its request built from the command line, the
//! operation called through `dyn StoreOps` (spec §7), or, for `git import` and `git export`, a
//! client-side operation that no `StoreOps` has, on the store's `LocalOps` ([`Ctx::on_local`]),
//! and its envelope rendered. The git remotes' commands (M13) are client-side operations too:
//! `remote`, `fetch`, `push` and `sync` run on the store's `LocalOps` as `git import` does, and
//! `clone`, which has no store to open yet, runs as `init` does.
//!
//! A command first checks what it can without the store: the store the user named (only
//! whether the variable is readable), a mutation's identity, its own arguments and a commit's
//! change list. Then it opens the store and runs the operation. So a malformed invocation
//! fails the same way whatever the store holds, and costs no store access.
//!
//! The envelope's `meta`: `repo` is the store as the user named it (for `init`, its
//! directory), omitted when the command uses no store (`whoami`, `schema`) or none was named;
//! `elapsed_ms` spans the whole command but its printing: reading the command line, opening
//! the store and the operation; `resolved` is every revision the operation resolved, as the
//! command's own `Resolved` records them (`LocalOps::recording`), also when it failed; `auth` is
//! the store's authentication mode, `trust` in M1; `recovered`, only when there is something to
//! say, is what the operation's write repaired before its own work, as the same record has it
//! (on success and on `E_CAS_MISMATCH`).
//!
//! A panic's envelope has the same `repo`, `resolved` and `recovered` (see [`Used`]), or none of
//! them when it came before a store was named.
//!
//! `serve` (M14) prints no envelope on stdout, which carries git's protocol: a refusal before git
//! starts is one `ERR` pkt-line there, and its envelope goes to stderr (the `serve` module).
//!
//! `whoami` prints `IdentityInfo` as the API writes it: for a caller without an identity,
//! `{"auth":"anonymous"}`, with `agent` omitted rather than null.

mod run;
mod serve;
pub(crate) mod ws;

pub use self::run::{Ctx, Used, failed, failed_after, failed_envelope, panicked, run, usage};
pub(crate) use self::run::{Depth, clean, is_own_stdio};
