//! The git side of `swarmops serve` (M14 design §0): the serving repository `<store>/git/serve.git`
//! that host git's `upload-pack`, `receive-pack` and `http-backend` run on, the map between store
//! refs and git refs, git's pkt-line and `proc-receive` codecs, and the land key of a push to
//! `refs/land/<target>`.
//!
//! ```text
//! swarmops-ops serve ──▶ repo (serve.git, its lock and hook) ──▶ child (host git, one builder)
//!                    ├─▶ refmap (store ref ↔ git ref)
//!                    ├─▶ pkt, proc_receive (the hook's protocol)
//!                    └─▶ land_key (refs/land/<target>)
//! ```
//!
//! The contracts are frozen here; each module's body comes with the M14 plan's task that owns it,
//! and until then answers [`repo::ServeError::NotBuilt`] (or, where it returns no error, a value
//! that serves nothing).

pub mod child;
pub mod land_key;
pub mod pkt;
pub mod proc_receive;
pub mod refmap;
pub mod repo;
